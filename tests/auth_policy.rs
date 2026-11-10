//! SQL login password policy: complexity, lockout, expiration, MUST_CHANGE, LOGINPROPERTY.
mod common;
use common::*;

#[test]
fn complexity_is_enforced_with_check_policy() {
    let s = server();
    let mut sa = s.connect();
    assert_eq!(err_no(&mut sa, "CREATE LOGIN a1 WITH PASSWORD = 'short', CHECK_POLICY = ON"), 15118);
    assert_eq!(err_no(&mut sa, "CREATE LOGIN a1 WITH PASSWORD = 'alllowercase', CHECK_POLICY = ON"), 15118);
    assert_eq!(err_no(&mut sa, "CREATE LOGIN alice WITH PASSWORD = 'XAlice1!!', CHECK_POLICY = ON"), 15118, "contains the login name");
    exec(&mut sa, "CREATE LOGIN a1 WITH PASSWORD = 'Str0ng!pass', CHECK_POLICY = ON");
    // without a policy anything goes
    exec(&mut sa, "CREATE LOGIN a2 WITH PASSWORD = 'x', CHECK_POLICY = OFF");
    assert_eq!(err_no(&mut sa, "ALTER LOGIN a1 WITH PASSWORD = 'weak'"), 15118);
    exec(&mut sa, "ALTER LOGIN a1 WITH CHECK_POLICY = OFF");
    exec(&mut sa, "ALTER LOGIN a1 WITH PASSWORD = 'weak'");
}

#[test]
fn lockout_after_repeated_failures_and_unlock() {
    let s = server();
    let mut sa = s.connect();
    exec(&mut sa, "CREATE LOGIN u WITH PASSWORD = 'Str0ng!pass', CHECK_POLICY = ON");
    for _ in 0..4 {
        assert_eq!(s.connect_as("u", "bad").err().unwrap().number, 18456);
    }
    assert_eq!(scalar(&mut sa, "SELECT LOGINPROPERTY('u', 'BadPasswordCount')"), "4");
    assert!(s.connect_as("u", "Str0ng!pass").is_ok(), "a good login resets the count");
    assert_eq!(scalar(&mut sa, "SELECT LOGINPROPERTY('u', 'BadPasswordCount')"), "0");
    for _ in 0..5 {
        let _ = s.connect_as("u", "bad");
    }
    assert_eq!(scalar(&mut sa, "SELECT LOGINPROPERTY('u', 'IsLocked')"), "1");
    assert_eq!(s.connect_as("u", "Str0ng!pass").err().unwrap().number, 18486, "even the right password fails while locked");
    exec(&mut sa, "ALTER LOGIN u WITH PASSWORD = 'Str0ng!pass' UNLOCK");
    assert_eq!(scalar(&mut sa, "SELECT LOGINPROPERTY('u', 'IsLocked')"), "0");
    assert!(s.connect_as("u", "Str0ng!pass").is_ok());
}

#[test]
fn no_lockout_without_policy() {
    let s = server();
    let mut sa = s.connect();
    exec(&mut sa, "CREATE LOGIN u WITH PASSWORD = 'pw', CHECK_POLICY = OFF");
    for _ in 0..20 {
        let _ = s.connect_as("u", "bad");
    }
    assert!(s.connect_as("u", "pw").is_ok());
}

#[test]
fn must_change_blocks_login_until_password_changed() {
    let s = server();
    let mut sa = s.connect();
    exec(&mut sa, "CREATE LOGIN m WITH PASSWORD = 'Temp0rary!' MUST_CHANGE, CHECK_EXPIRATION = ON, CHECK_POLICY = ON");
    assert_eq!(scalar(&mut sa, "SELECT LOGINPROPERTY('m', 'IsMustChange')"), "1");
    assert_eq!(s.connect_as("m", "Temp0rary!").err().unwrap().number, 18488);
    exec(&mut sa, "ALTER LOGIN m WITH PASSWORD = 'Brand-N3w-pw'");
    assert_eq!(scalar(&mut sa, "SELECT LOGINPROPERTY('m', 'IsMustChange')"), "0");
    assert!(s.connect_as("m", "Brand-N3w-pw").is_ok());
}

#[test]
fn expiration_uses_the_configured_maximum_age() {
    let s = server();
    let mut sa = s.connect();
    exec(&mut sa, "CREATE LOGIN e WITH PASSWORD = 'Str0ng!pass', CHECK_POLICY = ON, CHECK_EXPIRATION = ON");
    assert_eq!(scalar(&mut sa, "SELECT LOGINPROPERTY('e', 'IsExpired')"), "0");
    assert!(s.connect_as("e", "Str0ng!pass").is_ok());
    assert_eq!(scalar(&mut sa, "SELECT CASE WHEN LOGINPROPERTY('e', 'PasswordLastSetTime') IS NULL THEN 0 ELSE 1 END"), "1");
    // a maximum age of one day cannot be exceeded by a fresh password; 0 means never
    exec(&mut sa, "EXEC sp_configure 'password max age (days)', 0");
    assert!(s.connect_as("e", "Str0ng!pass").is_ok());
}

#[test]
fn users_change_their_own_password_with_old_password() {
    let s = server();
    let mut sa = s.connect();
    exec(&mut sa, "CREATE LOGIN p WITH PASSWORD = 'Str0ng!pass', CHECK_POLICY = ON");
    let mut p = s.connect_as("p", "Str0ng!pass").unwrap();
    assert_eq!(err_no(&mut p, "ALTER LOGIN p WITH PASSWORD = 'An0ther!one'"), 15151, "OLD_PASSWORD is required");
    assert_eq!(err_no(&mut p, "ALTER LOGIN p WITH PASSWORD = 'An0ther!one' OLD_PASSWORD = 'wrong'"), 15151);
    exec(&mut p, "ALTER LOGIN p WITH PASSWORD = 'An0ther!one' OLD_PASSWORD = 'Str0ng!pass'");
    assert!(s.connect_as("p", "An0ther!one").is_ok());
}
