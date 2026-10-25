use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Result, bail, ensure};

const MAX_BUF: usize = 1024 * 1024;
const NOT_FOUND: [libc::c_int; 4] = [libc::ENOENT, libc::ESRCH, libc::EBADF, libc::EPERM];

struct Entry {
    pwd: libc::passwd,
    _buf: Vec<libc::c_char>,
}

fn initial_buf_len() -> usize {
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    usize::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1024)
        .min(MAX_BUF)
}

fn lookup(
    mut call: impl FnMut(
        *mut libc::passwd,
        *mut libc::c_char,
        libc::size_t,
        *mut *mut libc::passwd,
    ) -> libc::c_int,
) -> Result<Option<Entry>> {
    let mut len = initial_buf_len();
    loop {
        let mut buf = vec![0 as libc::c_char; len];
        // SAFETY: passwd is plain C data; all-zero is a valid value.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = call(&mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
        if rc == libc::ERANGE && len < MAX_BUF {
            len = (len * 2).min(MAX_BUF);
            continue;
        }
        if NOT_FOUND.contains(&rc) {
            return Ok(None);
        }
        ensure!(
            rc == 0,
            "user database lookup failed: {}",
            std::io::Error::from_raw_os_error(rc)
        );
        return Ok((!result.is_null()).then_some(Entry { pwd, _buf: buf }));
    }
}

fn by_uid(uid: u32) -> Result<Option<Entry>> {
    lookup(|pwd, buf, len, result| {
        // SAFETY: every pointer is valid for the call and `buf` is `len` bytes.
        unsafe { libc::getpwuid_r(uid, pwd, buf, len, result) }
    })
}

fn home_from_dir(dir: &[u8]) -> Result<PathBuf> {
    let home = PathBuf::from(OsStr::from_bytes(dir));
    ensure!(
        home.is_absolute(),
        "home directory {home:?} is not absolute"
    );
    Ok(home)
}

pub fn home_for_uid(uid: u32) -> Result<PathBuf> {
    let entry = by_uid(uid)?;
    let Some(dir) = entry
        .as_ref()
        .map(|e| e.pwd.pw_dir)
        .filter(|d| !d.is_null())
    else {
        bail!("no account for uid {uid}");
    };
    // SAFETY: `pw_dir` is a NUL-terminated string inside `entry`'s buffer, still alive.
    home_from_dir(unsafe { CStr::from_ptr(dir) }.to_bytes())
}

pub fn name_for_uid(uid: u32) -> Result<Option<String>> {
    let Some(entry) = by_uid(uid)? else {
        return Ok(None);
    };
    // SAFETY: `pw_name` is a NUL-terminated string inside `entry`'s buffer, still alive.
    let name = unsafe { CStr::from_ptr(entry.pwd.pw_name) };
    Ok(name.to_str().ok().map(str::to_string))
}

pub fn uid_for_name(name: &str) -> Result<Option<u32>> {
    let Ok(cname) = CString::new(name) else {
        return Ok(None);
    };
    let entry = lookup(|pwd, buf, len, result| {
        // SAFETY: every pointer is valid for the call and `buf` is `len` bytes.
        unsafe { libc::getpwnam_r(cname.as_ptr(), pwd, buf, len, result) }
    })?;
    Ok(entry.map(|e| e.pwd.pw_uid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(pwd: *mut libc::passwd, result: *mut *mut libc::passwd) -> libc::c_int {
        // SAFETY: `result` points at the out-pointer `lookup` owns for this call.
        unsafe { *result = pwd };
        0
    }

    #[test]
    fn home_for_uid_resolves_the_current_user_home() {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let home = home_for_uid(uid).unwrap();
        assert!(home.is_absolute());
        assert!(!home.as_os_str().is_empty());
    }

    #[test]
    fn home_for_uid_rejects_a_uid_with_no_account() {
        assert!(home_for_uid(3_999_999_999).is_err());
    }

    #[test]
    fn an_empty_or_relative_home_is_rejected() {
        assert!(home_from_dir(b"").is_err());
        assert!(home_from_dir(b"relative/home").is_err());
        assert_eq!(home_from_dir(b"/home/x").unwrap(), PathBuf::from("/home/x"));
    }

    #[test]
    fn erange_grows_the_buffer_until_the_entry_fits() {
        let mut seen = Vec::new();
        let entry = lookup(|pwd, _, len, result| {
            seen.push(len);
            if len < 64 * 1024 {
                libc::ERANGE
            } else {
                found(pwd, result)
            }
        })
        .unwrap();
        assert!(entry.is_some());
        assert!(seen.windows(2).all(|w| w[1] == w[0] * 2));
    }

    #[test]
    fn erange_past_the_cap_is_an_error() {
        let mut largest = 0;
        assert!(
            lookup(|_, _, len, _| {
                largest = len;
                libc::ERANGE
            })
            .is_err()
        );
        assert!(largest <= 1024 * 1024);
    }

    #[test]
    fn not_found_errnos_mean_no_such_user() {
        for errno in [libc::ENOENT, libc::ESRCH, libc::EBADF, libc::EPERM] {
            assert!(lookup(|_, _, _, _| errno).unwrap().is_none());
        }
        assert!(lookup(|_, _, _, _| libc::EIO).is_err());
    }

    #[test]
    fn name_and_uid_round_trip_for_the_current_user() {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let name = name_for_uid(uid).unwrap().unwrap();
        assert_eq!(uid_for_name(&name).unwrap(), Some(uid));
    }

    #[test]
    fn unknown_names_and_uids_resolve_to_none() {
        assert_eq!(uid_for_name("no-such-user-x").unwrap(), None);
        assert_eq!(uid_for_name("bad\0name").unwrap(), None);
        assert_eq!(name_for_uid(3_999_999_999).unwrap(), None);
    }
}
