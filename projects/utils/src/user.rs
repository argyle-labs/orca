use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Result, ensure};

struct Entry {
    pwd: libc::passwd,
    _buf: Vec<libc::c_char>,
}

fn lookup(
    call: impl FnOnce(
        *mut libc::passwd,
        *mut libc::c_char,
        libc::size_t,
        *mut *mut libc::passwd,
    ) -> libc::c_int,
) -> Result<Option<Entry>> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = call(&mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
    ensure!(
        rc == 0,
        "user database lookup failed: {}",
        std::io::Error::from_raw_os_error(rc)
    );
    Ok((!result.is_null()).then_some(Entry { pwd, _buf: buf }))
}

fn by_uid(uid: u32) -> Result<Option<Entry>> {
    lookup(|pwd, buf, len, result| unsafe { libc::getpwuid_r(uid, pwd, buf, len, result) })
}

pub fn home_for_uid(uid: u32) -> Result<PathBuf> {
    let entry = by_uid(uid)?;
    let dir = entry
        .as_ref()
        .map(|e| e.pwd.pw_dir)
        .filter(|d| !d.is_null());
    let Some(dir) = dir else {
        anyhow::bail!("no account for uid {uid}");
    };
    let dir = unsafe { CStr::from_ptr(dir) };
    Ok(PathBuf::from(OsStr::from_bytes(dir.to_bytes())))
}

pub fn name_for_uid(uid: u32) -> Option<String> {
    let entry = by_uid(uid).ok()??;
    let name = unsafe { CStr::from_ptr(entry.pwd.pw_name) };
    name.to_str().ok().map(str::to_string)
}

pub fn uid_for_name(name: &str) -> Option<u32> {
    let cname = CString::new(name).ok()?;
    let entry = lookup(|pwd, buf, len, result| unsafe {
        libc::getpwnam_r(cname.as_ptr(), pwd, buf, len, result)
    })
    .ok()??;
    Some(entry.pwd.pw_uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_for_uid_resolves_the_current_user_home() {
        let uid = unsafe { libc::getuid() };
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(home_for_uid(uid).unwrap(), home);
    }

    #[test]
    fn home_for_uid_rejects_a_uid_with_no_account() {
        assert!(home_for_uid(3_999_999_999).is_err());
    }

    #[test]
    fn name_and_uid_round_trip_for_the_current_user() {
        let uid = unsafe { libc::geteuid() };
        let name = name_for_uid(uid).unwrap();
        assert_eq!(uid_for_name(&name), Some(uid));
    }

    #[test]
    fn unknown_names_and_uids_resolve_to_none() {
        assert_eq!(uid_for_name("no-such-user-x"), None);
        assert_eq!(uid_for_name("bad\0name"), None);
        assert_eq!(name_for_uid(3_999_999_999), None);
    }
}
