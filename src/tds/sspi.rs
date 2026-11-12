//! Kerberos on Windows through SSPI (the `Negotiate` package, which picks Kerberos when the client can use it). Built with
//! the feature `kerberos-windows`.
//!
//! `Negotiate` falls back to NTLM when Kerberos is not possible (no domain, or the client cannot find the service principal);
//! that is refused unless [`KerberosConfig::allow_ntlm`] is set, so that "Kerberos" means Kerberos. The authenticated
//! user is `DOMAIN\user`, the name a `CREATE LOGIN [DOMAIN\user] FROM WINDOWS` uses.
use super::auth::{Acceptor, KerberosConfig};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Security::Authentication::Identity::{
    AcceptSecurityContext, AcquireCredentialsHandleW, DeleteSecurityContext, FreeContextBuffer, FreeCredentialsHandle, QueryContextAttributesW, SecBuffer, SecBufferDesc,
};
use windows_sys::Win32::Security::Credentials::SecHandle;

const SECPKG_CRED_INBOUND: u32 = 1;
const SECBUFFER_VERSION: u32 = 0;
const SECBUFFER_TOKEN: u32 = 2;
const SECURITY_NATIVE_DREP: u32 = 0x10;
const ASC_REQ_ALLOCATE_MEMORY: u32 = 0x100;
const ASC_REQ_CONNECTION: u32 = 0x800;
const SECPKG_ATTR_NAMES: u32 = 1;
const SECPKG_ATTR_NEGOTIATION_INFO: u32 = 12;
const SEC_E_OK: i32 = 0;
const SEC_I_CONTINUE_NEEDED: i32 = 0x0009_0312;

#[repr(C)]
struct NamesW {
    user_name: *mut u16,
}

#[repr(C)]
struct PkgInfoW {
    capabilities: u32,
    version: u16,
    rpcid: u16,
    max_token: u32,
    name: *mut u16,
    comment: *mut u16,
}

#[repr(C)]
struct NegotiationInfoW {
    package_info: *mut PkgInfoW,
    negotiation_state: u32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

pub struct SspiAcceptor {
    cred: SecHandle,
    ctx: SecHandle,
    have_ctx: bool,
    allow_ntlm: bool,
}

// the handles are used by one thread at a time (a login runs on its connection's thread)
unsafe impl Send for SspiAcceptor {}

impl SspiAcceptor {
    pub fn new(cfg: &KerberosConfig) -> Result<SspiAcceptor, String> {
        let mut cred = SecHandle { dwLower: 0, dwUpper: 0 };
        let mut expiry = 0i64;
        let package = wide("Negotiate");
        // SAFETY: the arguments are valid for the call
        let r = unsafe { AcquireCredentialsHandleW(null(), package.as_ptr(), SECPKG_CRED_INBOUND, null(), null(), None, null(), &mut cred, &mut expiry) };
        if r != SEC_E_OK {
            return Err(format!("AcquireCredentialsHandle failed: {:#x}", r as u32));
        }
        Ok(SspiAcceptor { cred, ctx: SecHandle { dwLower: 0, dwUpper: 0 }, have_ctx: false, allow_ntlm: cfg.allow_ntlm })
    }

    fn package_name(&self) -> String {
        let mut info = NegotiationInfoW { package_info: null_mut(), negotiation_state: 0 };
        // SAFETY: the context is complete; the package info is allocated by SSPI and freed below
        unsafe {
            if QueryContextAttributesW(&self.ctx, SECPKG_ATTR_NEGOTIATION_INFO, &mut info as *mut _ as *mut c_void) != SEC_E_OK || info.package_info.is_null() {
                return String::new();
            }
            let name = from_wide((*info.package_info).name);
            FreeContextBuffer(info.package_info as *mut c_void);
            name
        }
    }

    fn user_name(&self) -> Result<String, String> {
        let mut names = NamesW { user_name: null_mut() };
        // SAFETY: as above
        unsafe {
            let r = QueryContextAttributesW(&self.ctx, SECPKG_ATTR_NAMES, &mut names as *mut _ as *mut c_void);
            if r != SEC_E_OK || names.user_name.is_null() {
                return Err(format!("the client's name is unknown: {:#x}", r as u32));
            }
            let s = from_wide(names.user_name);
            FreeContextBuffer(names.user_name as *mut c_void);
            Ok(s)
        }
    }
}

impl Acceptor for SspiAcceptor {
    fn step(&mut self, token: &[u8]) -> Result<(Vec<u8>, Option<Vec<String>>), String> {
        let mut in_buf = SecBuffer { cbBuffer: token.len() as u32, BufferType: SECBUFFER_TOKEN, pvBuffer: token.as_ptr() as *mut c_void };
        let in_desc = SecBufferDesc { ulVersion: SECBUFFER_VERSION, cBuffers: 1, pBuffers: &mut in_buf };
        let mut out_buf = SecBuffer { cbBuffer: 0, BufferType: SECBUFFER_TOKEN, pvBuffer: null_mut() };
        let mut out_desc = SecBufferDesc { ulVersion: SECBUFFER_VERSION, cBuffers: 1, pBuffers: &mut out_buf };
        let mut new_ctx = SecHandle { dwLower: 0, dwUpper: 0 };
        let (mut attrs, mut expiry) = (0u32, 0i64);
        let ctx_in: *const SecHandle = if self.have_ctx { &self.ctx } else { null() };
        // SAFETY: the buffers are valid for the call; the output token is allocated by SSPI and freed below
        let r = unsafe { AcceptSecurityContext(&self.cred, ctx_in, &in_desc, ASC_REQ_ALLOCATE_MEMORY | ASC_REQ_CONNECTION, SECURITY_NATIVE_DREP, &mut new_ctx, &mut out_desc, &mut attrs, &mut expiry) };
        let out = if out_buf.pvBuffer.is_null() { vec![] } else { unsafe { std::slice::from_raw_parts(out_buf.pvBuffer as *const u8, out_buf.cbBuffer as usize) }.to_vec() };
        if !out_buf.pvBuffer.is_null() {
            unsafe { FreeContextBuffer(out_buf.pvBuffer) };
        }
        if r != SEC_E_OK && r != SEC_I_CONTINUE_NEEDED {
            return Err(format!("AcceptSecurityContext failed: {:#x}", r as u32));
        }
        self.ctx = new_ctx;
        self.have_ctx = true;
        if r == SEC_I_CONTINUE_NEEDED {
            return Ok((out, None));
        }
        let package = self.package_name();
        if !self.allow_ntlm && !package.eq_ignore_ascii_case("Kerberos") {
            return Err(format!("the client authenticated with {} and not Kerberos (the server needs a domain account and a resolvable service principal; --allow-ntlm accepts NTLM)", if package.is_empty() { "an unknown package" } else { &package }));
        }
        Ok((out, Some(vec![self.user_name()?])))
    }
}

impl Drop for SspiAcceptor {
    fn drop(&mut self) {
        // SAFETY: the handles are ours
        unsafe {
            if self.have_ctx {
                DeleteSecurityContext(&self.ctx);
            }
            FreeCredentialsHandle(&self.cred);
        }
    }
}
