//! Kerberos through GSSAPI: MIT Kerberos (`libgssapi_krb5`) on Linux, `GSS.framework` on macOS. Built with the feature
//! `kerberos-linux` or `kerberos-macos`.
//!
//! The functions of the C library are declared here by hand (the API is small and stable, and the two platforms export the
//! same names), so the build needs only the library to link against: `libkrb5-dev` (Debian, Ubuntu) or `krb5-devel`
//! (Fedora, RHEL) on Linux, and nothing extra on macOS.
use super::auth::{Acceptor, KerberosConfig};
use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr::null_mut;

#[cfg(all(feature = "kerberos-linux", not(target_os = "linux")))]
compile_error!("the feature kerberos-linux builds only for Linux; use kerberos-windows or kerberos-macos here");
#[cfg(all(feature = "kerberos-macos", not(target_os = "macos")))]
compile_error!("the feature kerberos-macos builds only for macOS; use kerberos-windows or kerberos-linux here");

#[repr(C)]
struct Buffer {
    length: usize,
    value: *mut c_void,
}

#[repr(C)]
struct OidDesc {
    length: u32,
    elements: *mut c_void,
}

type Oid = *mut OidDesc;
type Ctx = *mut c_void;
type Cred = *mut c_void;
type Name = *mut c_void;

#[cfg_attr(target_os = "linux", link(name = "gssapi_krb5"))]
#[cfg_attr(target_os = "macos", link(name = "GSS", kind = "framework"))]
extern "C" {
    fn gss_import_name(minor: *mut u32, input: *mut Buffer, name_type: Oid, out: *mut Name) -> u32;
    fn gss_acquire_cred(minor: *mut u32, name: Name, time_req: u32, mechs: *mut c_void, usage: c_int, out: *mut Cred, actual: *mut *mut c_void, time_rec: *mut u32) -> u32;
    #[allow(clippy::too_many_arguments)]
    fn gss_accept_sec_context(minor: *mut u32, ctx: *mut Ctx, cred: Cred, input: *mut Buffer, chan: *mut c_void, src_name: *mut Name, mech: *mut Oid, output: *mut Buffer, flags: *mut u32, time_rec: *mut u32, deleg: *mut Cred) -> u32;
    fn gss_display_name(minor: *mut u32, name: Name, out: *mut Buffer, name_type: *mut Oid) -> u32;
    fn gss_display_status(minor: *mut u32, status: u32, kind: c_int, mech: Oid, message_context: *mut u32, out: *mut Buffer) -> u32;
    fn gss_release_buffer(minor: *mut u32, buf: *mut Buffer) -> u32;
    fn gss_release_name(minor: *mut u32, name: *mut Name) -> u32;
    fn gss_release_cred(minor: *mut u32, cred: *mut Cred) -> u32;
    fn gss_delete_sec_context(minor: *mut u32, ctx: *mut Ctx, out: *mut Buffer) -> u32;
}

const GSS_C_ACCEPT: c_int = 2;
const GSS_C_GSS_CODE: c_int = 1;
const GSS_C_MECH_CODE: c_int = 2;
const CONTINUE_NEEDED: u32 = 1;
const ROUTINE_ERROR_MASK: u32 = 0xFFFF_0000;

/// The OID of the Kerberos principal name type, 1.2.840.113554.1.2.2.1 (`user@REALM`, `MSSQLSvc/host:1433@REALM`).
static KRB5_NT_PRINCIPAL_BYTES: [u8; 10] = [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x12, 0x01, 0x02, 0x02, 0x01];

/// The text of a GSSAPI failure.
fn describe(major: u32, minor: u32) -> String {
    let mut out = vec![];
    for (code, kind) in [(major, GSS_C_GSS_CODE), (minor, GSS_C_MECH_CODE)] {
        let mut ctx = 0u32;
        loop {
            let (mut m, mut buf) = (0u32, Buffer { length: 0, value: null_mut() });
            // SAFETY: the buffer is filled by the library and released below
            let r = unsafe { gss_display_status(&mut m, code, kind, null_mut(), &mut ctx, &mut buf) };
            if r & ROUTINE_ERROR_MASK != 0 {
                break;
            }
            if !buf.value.is_null() {
                let s = unsafe { std::slice::from_raw_parts(buf.value as *const u8, buf.length) };
                out.push(String::from_utf8_lossy(s).into_owned());
                unsafe { gss_release_buffer(&mut m, &mut buf) };
            }
            if ctx == 0 {
                break;
            }
        }
    }
    if out.is_empty() {
        format!("GSSAPI error {:#x}/{}", major, minor)
    } else {
        out.join("; ")
    }
}

pub struct GssAcceptor {
    ctx: Ctx,
    cred: Cred,
}

// the handles are used by one thread at a time (a login runs on its connection's thread)
unsafe impl Send for GssAcceptor {}

impl GssAcceptor {
    pub fn new(cfg: &KerberosConfig) -> Result<GssAcceptor, String> {
        if let Some(k) = &cfg.keytab {
            std::env::set_var("KRB5_KTNAME", k);
        }
        let mut cred: Cred = null_mut();
        if let Some(service) = &cfg.service {
            // accept only for this service principal
            let mut bytes = service.clone().into_bytes();
            let mut buf = Buffer { length: bytes.len(), value: bytes.as_mut_ptr() as *mut c_void };
            let mut oid = OidDesc { length: KRB5_NT_PRINCIPAL_BYTES.len() as u32, elements: KRB5_NT_PRINCIPAL_BYTES.as_ptr() as *mut c_void };
            let (mut minor, mut name): (u32, Name) = (0, null_mut());
            // SAFETY: the buffers outlive the calls; the name is released after the credential is acquired
            let major = unsafe { gss_import_name(&mut minor, &mut buf, &mut oid, &mut name) };
            if major & ROUTINE_ERROR_MASK != 0 {
                return Err(format!("the service name '{}' is not valid: {}", service, describe(major, minor)));
            }
            let major = unsafe { gss_acquire_cred(&mut minor, name, 0, null_mut(), GSS_C_ACCEPT, &mut cred, null_mut(), null_mut()) };
            unsafe { gss_release_name(&mut minor, &mut name) };
            if major & ROUTINE_ERROR_MASK != 0 {
                return Err(format!("no credentials for '{}' (is the key in the keytab?): {}", service, describe(major, minor)));
            }
        }
        Ok(GssAcceptor { ctx: null_mut(), cred })
    }
}

impl Acceptor for GssAcceptor {
    fn step(&mut self, token: &[u8]) -> Result<(Vec<u8>, Option<Vec<String>>), String> {
        let mut input = Buffer { length: token.len(), value: token.as_ptr() as *mut c_void };
        let mut output = Buffer { length: 0, value: null_mut() };
        let (mut minor, mut src): (u32, Name) = (0, null_mut());
        // SAFETY: all pointers are valid for the call; the output buffer and the name are released below
        let major = unsafe { gss_accept_sec_context(&mut minor, &mut self.ctx, self.cred, &mut input, null_mut(), &mut src, null_mut(), &mut output, null_mut(), null_mut(), null_mut()) };
        let out = if output.value.is_null() { vec![] } else { unsafe { std::slice::from_raw_parts(output.value as *const u8, output.length) }.to_vec() };
        if !output.value.is_null() {
            unsafe { gss_release_buffer(&mut minor, &mut output) };
        }
        if major & ROUTINE_ERROR_MASK != 0 {
            return Err(describe(major, minor));
        }
        if major & CONTINUE_NEEDED != 0 {
            return Ok((out, None));
        }
        // complete: who is it
        let mut disp = Buffer { length: 0, value: null_mut() };
        let major = unsafe { gss_display_name(&mut minor, src, &mut disp, null_mut()) };
        if major & ROUTINE_ERROR_MASK != 0 || disp.value.is_null() {
            unsafe { gss_release_name(&mut minor, &mut src) };
            return Err(format!("the client's name is unknown: {}", describe(major, minor)));
        }
        let principal = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(disp.value as *const u8, disp.length) }).into_owned();
        unsafe {
            gss_release_buffer(&mut minor, &mut disp);
            gss_release_name(&mut minor, &mut src);
        }
        Ok((out, Some(principal_names(&principal))))
    }
}

impl Drop for GssAcceptor {
    fn drop(&mut self) {
        let mut minor = 0u32;
        // SAFETY: the handles are ours; the library tolerates null
        unsafe {
            if !self.ctx.is_null() {
                gss_delete_sec_context(&mut minor, &mut self.ctx, null_mut());
            }
            if !self.cred.is_null() {
                gss_release_cred(&mut minor, &mut self.cred);
            }
        }
    }
}

/// The logins a principal `user@REALM` can be known by: `REALM\user` (the realm's first label, as the NetBIOS domain name
/// of an Active Directory domain) and the principal itself.
pub fn principal_names(principal: &str) -> Vec<String> {
    let mut names = vec![];
    if let Some((user, realm)) = principal.split_once('@') {
        let short = realm.split('.').next().unwrap_or(realm);
        names.push(format!("{}\\{}", short, user));
    }
    names.push(principal.to_string());
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_principal_is_known_by_domain_and_by_realm_forms() {
        assert_eq!(principal_names("alice@EXAMPLE.COM"), vec!["EXAMPLE\\alice".to_string(), "alice@EXAMPLE.COM".to_string()]);
        assert_eq!(principal_names("bob"), vec!["bob".to_string()]);
    }
}
