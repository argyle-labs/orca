//! Confinement of read-role `files.*` to registered roots.
//!
//! A path is opened one component at a time with `openat(O_NOFOLLOW)` from the
//! root's directory fd, so a `..`, a symlink, or a rename racing the check
//! cannot leave the root: what was checked is the fd that gets read.

use anyhow::{Result, anyhow};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

fn escape(rel: &Path, why: &str) -> anyhow::Error {
    contract::OrcaError::forbidden(format!(
        "files: {} {why} its registered root",
        rel.display()
    ))
    .into()
}

fn open_at(dir: Option<&OwnedFd>, name: &Path, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    let c = CString::new(name.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let base = dir.map_or(libc::AT_FDCWD, |d| d.as_raw_fd());
    // O_NONBLOCK: opening a FIFO planted in a root must not hang the daemon.
    let flags = flags | libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: `c` is a valid NUL-terminated string; `base` is AT_FDCWD or a
    // live fd borrowed from `dir`.
    let fd = unsafe { libc::openat(base, c.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by openat and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open `rel` beneath `root` without following any symlink or `..` out of it.
/// Only regular files and directories are returned. A missing component
/// surfaces as an `io::Error` of kind `NotFound`; an escape is `Forbidden`.
pub fn open_beneath(root: &Path, rel: &Path) -> Result<File> {
    let mut stack = vec![open_at(None, root, libc::O_DIRECTORY)?];
    let comps: Vec<Component> = rel.components().collect();
    for (i, comp) in comps.iter().enumerate() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                return Err(escape(rel, "is absolute, not relative to"));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.len() == 1 {
                    return Err(escape(rel, "climbs out of"));
                }
                stack.pop();
            }
            Component::Normal(name) => {
                let last = i + 1 == comps.len();
                let flags = libc::O_NOFOLLOW | if last { 0 } else { libc::O_DIRECTORY };
                let top = stack.last().expect("stack holds the root");
                match open_at(Some(top), Path::new(name), flags) {
                    Ok(fd) => stack.push(fd),
                    // ELOOP: a symlink under O_NOFOLLOW. ENOTDIR here can be a
                    // symlink-to-dir mid-path on macOS.
                    Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                        return Err(escape(rel, "passes through a symlink in"));
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) && !last => {
                        let lstat = std::fs::symlink_metadata(
                            root.join(comps[..=i].iter().collect::<PathBuf>()),
                        );
                        if lstat.is_ok_and(|m| m.file_type().is_symlink()) {
                            return Err(escape(rel, "passes through a symlink in"));
                        }
                        return Err(e.into());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    let file = File::from(stack.pop().expect("stack holds the root"));
    let ft = file.metadata()?.file_type();
    if !ft.is_file() && !ft.is_dir() {
        return Err(contract::OrcaError::forbidden(format!(
            "files: {} is not a regular file or directory",
            rel.display()
        ))
        .into());
    }
    Ok(file)
}

/// The part of absolute `path` beneath `root`, under either the root's
/// registered spelling or its canonical one. Only picks the starting fd;
/// `open_beneath` is what enforces containment.
pub fn relative_to_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = root.canonicalize().ok();
    [Some(root.to_path_buf()), canonical]
        .into_iter()
        .flatten()
        .find_map(|r| path.strip_prefix(&r).ok().map(Path::to_path_buf))
}

pub fn outside_roots(path: &Path) -> anyhow::Error {
    contract::OrcaError::forbidden(format!(
        "files: {} is outside every registered root",
        path.display()
    ))
    .into()
}

/// Entry names of the directory open as `dir`, read through the fd.
pub fn read_dir_names(dir: &File) -> Result<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStringExt;
    // SAFETY: dup of a live fd; fdopendir takes ownership of the duplicate,
    // released by closedir below.
    let dup = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err.into());
    }
    // A dup shares the offset; rewind so a reused fd lists from the start.
    unsafe { libc::rewinddir(dirp) };
    let mut names = Vec::new();
    loop {
        // SAFETY: `dirp` is a valid DIR*; the entry is copied out before the
        // next readdir call invalidates it.
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
        }
    }
    unsafe { libc::closedir(dirp) };
    Ok(names)
}

/// `lstat` of `name` inside the directory open as `dir`.
pub fn lstat_at(dir: &File, name: &std::ffi::OsStr) -> Result<libc::stat> {
    let c = CString::new(name.as_bytes()).map_err(|e| anyhow!(e))?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid fd, NUL-terminated name, and an out-pointer sized for stat.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fstatat returned 0, so it filled `st`.
    Ok(unsafe { st.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;

    fn scratch(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("orca-confine-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn assert_forbidden(res: Result<File>) {
        let err = res.expect_err("must be refused");
        let oe = err
            .downcast_ref::<contract::OrcaError>()
            .unwrap_or_else(|| panic!("not an OrcaError: {err}"));
        assert_eq!(oe.kind, contract::ErrorKind::Forbidden, "{oe}");
    }

    #[test]
    fn opens_files_and_dirs_beneath_the_root() {
        let t = scratch("ok");
        fs::create_dir(t.path().join("a")).unwrap();
        fs::write(t.path().join("a").join("f.md"), "hi").unwrap();
        let mut s = String::new();
        open_beneath(t.path(), Path::new("a/f.md"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "hi");
        // `..` that stays inside the root is fine.
        open_beneath(t.path(), Path::new("a/../a/./f.md")).unwrap();
        assert!(
            open_beneath(t.path(), Path::new(""))
                .unwrap()
                .metadata()
                .unwrap()
                .is_dir()
        );
    }

    #[test]
    fn dot_dot_out_of_the_root_is_refused() {
        let t = scratch("dotdot");
        let root = t.path().join("root");
        fs::create_dir_all(root.join("x")).unwrap();
        fs::write(t.path().join("secret"), "s").unwrap();
        assert_forbidden(open_beneath(&root, Path::new("../secret")));
        assert_forbidden(open_beneath(&root, Path::new("x/../../secret")));
    }

    #[test]
    fn absolute_rel_is_refused() {
        let t = scratch("abs");
        assert_forbidden(open_beneath(t.path(), Path::new("/etc/hosts")));
    }

    #[test]
    fn symlinks_are_refused_final_and_mid_path() {
        let t = scratch("link");
        let root = t.path().join("root");
        let outside = t.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("f")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();
        assert_forbidden(open_beneath(&root, Path::new("f")));
        assert_forbidden(open_beneath(&root, Path::new("d/secret")));
        assert_forbidden(open_beneath(&root, Path::new("d")));
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let t = scratch("fifo");
        let p = CString::new(t.path().join("pipe").as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(p.as_ptr(), 0o600) }, 0);
        assert_forbidden(open_beneath(t.path(), Path::new("pipe")));
    }

    #[test]
    fn missing_is_not_found() {
        let t = scratch("missing");
        let err = open_beneath(t.path(), Path::new("nope")).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn relative_to_root_matches_either_spelling() {
        let t = scratch("rel");
        let canon = t.path().canonicalize().unwrap();
        assert_eq!(
            relative_to_root(t.path(), &t.path().join("a/b")).unwrap(),
            Path::new("a/b")
        );
        assert_eq!(
            relative_to_root(t.path(), &canon.join("a")).unwrap(),
            Path::new("a")
        );
        assert!(relative_to_root(t.path(), Path::new("/etc/hosts")).is_none());
    }
}
