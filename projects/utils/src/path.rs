use std::path::Path;

/// Expand a leading `~/` to the user's `$HOME` directory. If `$HOME` is
/// unset, the tilde is replaced with an empty string (matching prior
/// per-crate copies — callers already handle the unusual no-HOME case).
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

/// Locate an executable on `$PATH`. Returns the resolved absolute path, or
/// `None` if not found. For an existence check, `which(name).is_some()`.
///
/// Resolved IN-PROCESS. This used to shell out to the system `which`, which
/// made the lookup depend on `which(1)` itself being on `$PATH`: where it was
/// not — a minimal container, or any caller that narrowed `$PATH` — the spawn
/// failed and EVERY binary was reported missing, including ones plainly
/// present. A probe that answers "absent" when it means "I could not look" is
/// the failure mode worth removing, and it also drops a subprocess per lookup.
///
/// A name containing a separator is treated as a path and checked directly,
/// matching POSIX `command -v`.
pub fn which(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.contains(std::path::MAIN_SEPARATOR) {
        let p = Path::new(name);
        return is_executable_file(p).then(|| p.to_string_lossy().into_owned());
    }
    resolve_bin(name, None, std::env::var_os("PATH").as_deref(), &[])
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

pub fn resolve_bin(
    name: &str,
    override_bin: Option<&Path>,
    path_env: Option<&std::ffi::OsStr>,
    well_known: &[&Path],
) -> Result<std::path::PathBuf, Vec<String>> {
    let mut tried = Vec::new();
    if let Some(explicit) = override_bin.filter(|p| !p.as_os_str().is_empty()) {
        if is_executable_file(explicit) {
            return Ok(explicit.to_path_buf());
        }
        return Err(vec![explicit.to_string_lossy().into_owned()]);
    }
    let dirs = path_env.map(std::env::split_paths).into_iter().flatten();
    for candidate in dirs
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .chain(well_known.iter().map(|p| p.to_path_buf()))
    {
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
        tried.push(candidate.to_string_lossy().into_owned());
    }
    Err(tried)
}

#[cfg(unix)]
pub fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

pub const PRIVILEGED_BIN: &str = "/usr/local/libexec/orca/orca";

pub fn privileged_exe() -> std::io::Result<std::path::PathBuf> {
    pick_privileged(Path::new(PRIVILEGED_BIN).is_file())
}

fn pick_privileged(installed: bool) -> std::io::Result<std::path::PathBuf> {
    if installed {
        Ok(PRIVILEGED_BIN.into())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "the root-owned helper {PRIVILEGED_BIN} is missing; run `sudo orca identity privilege repair`"
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn privileged_exe_is_only_the_installed_root_copy() {
        assert_eq!(pick_privileged(true).unwrap(), Path::new(PRIVILEGED_BIN));
        let err = pick_privileged(false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string()
                .contains("sudo orca identity privilege repair"),
            "{err}"
        );
    }

    // Both `HOME`-mutating tests below touch the same process-global env var.
    // Serialize them behind one lock so the parallel test runner can't race one
    // test's `set_var`/`remove_var` against the other's assertions (an env-var
    // race that flaked the suite non-deterministically). Poison-tolerant: a
    // panicking test must not wedge the other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn expand_tilde_replaces_leading_tilde_with_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: setting HOME for the duration of this test; restored after.
        let prev = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", "/tmp/fakehome") };
        assert_eq!(expand_tilde("~/foo/bar"), "/tmp/fakehome/foo/bar");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn expand_tilde_without_home_uses_empty_string() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HOME").ok();
        unsafe { std::env::remove_var("HOME") };
        assert_eq!(expand_tilde("~/x"), "/x");
        if let Some(v) = prev {
            unsafe { std::env::set_var("HOME", v) }
        }
    }

    #[test]
    fn which_finds_common_binary() {
        // `sh` exists on every supported platform.
        let result = which("sh");
        assert!(result.is_some(), "expected to resolve `sh` on PATH");
        let path = result.unwrap();
        assert!(path.ends_with("sh"), "unexpected path: {path}");
    }

    #[test]
    fn which_returns_none_for_missing_binary() {
        assert!(which("this-binary-should-not-exist-orca-test-xyz").is_none());
    }

    #[test]
    fn a_narrowed_path_still_resolves_what_is_on_it() {
        // The bug this replaced: `which` shelled out to the system `which`, so
        // a PATH not containing `which(1)` made the spawn fail and EVERY
        // lookup report "not found" — including binaries plainly present.
        // A probe that answers "absent" when it means "I could not look" is
        // the failure worth removing.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("orca-fake-tool");
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let prior = std::env::var_os("PATH");
        // SAFETY: serialized behind ENV_LOCK with the other env-mutating tests.
        unsafe { std::env::set_var("PATH", dir.path()) };
        let found = which("orca-fake-tool");
        let missing = which("orca-definitely-absent");
        match prior {
            // SAFETY: as above.
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(
            found.as_deref(),
            exe.to_str(),
            "must resolve on a bare PATH"
        );
        assert!(missing.is_none());
    }

    #[test]
    fn a_non_executable_file_is_not_a_command() {
        // Right name, no execute bit. Treating it as a command turns a clear
        // "not found" into a confusing spawn failure further along.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("orca-not-exec");
        std::fs::write(&f, "data").expect("write");
        let prior = std::env::var_os("PATH");
        // SAFETY: serialized behind ENV_LOCK.
        unsafe { std::env::set_var("PATH", dir.path()) };
        let got = which("orca-not-exec");
        match prior {
            // SAFETY: as above.
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        #[cfg(unix)]
        assert!(got.is_none(), "{got:?}");
        #[cfg(not(unix))]
        let _ = got;
    }

    const ABSENT: &[&str] = &[
        "/nonexistent-dir/opt-homebrew/op",
        "/nonexistent-dir/usr-local/op",
    ];

    fn absent() -> Vec<&'static Path> {
        ABSENT.iter().map(Path::new).collect()
    }

    fn exec_stub(dir: &Path, name: &str, mode: u32) -> std::path::PathBuf {
        let bin = dir.join(name);
        std::fs::write(&bin, b"#!/bin/sh\n").expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("chmod");
        }
        #[cfg(not(unix))]
        let _ = mode;
        bin
    }

    #[test]
    fn resolve_bin_override_wins() {
        let got = resolve_bin(
            "op",
            Some(Path::new("/bin/sh")),
            Some(std::ffi::OsStr::new("/bin:/usr/bin")),
            &absent(),
        );
        assert_eq!(got.unwrap(), Path::new("/bin/sh"));
    }

    #[test]
    fn resolve_bin_bogus_override_fails_without_falling_through() {
        let err = resolve_bin(
            "sh",
            Some(Path::new("/nope/not/op")),
            Some(std::ffi::OsStr::new("/bin")),
            &[Path::new("/bin/sh")],
        )
        .unwrap_err();
        assert_eq!(err, vec!["/nope/not/op".to_string()]);
    }

    #[test]
    fn resolve_bin_empty_override_is_ignored() {
        let got = resolve_bin("sh", Some(Path::new("")), None, &[Path::new("/bin/sh")]);
        assert_eq!(got.unwrap(), Path::new("/bin/sh"));
    }

    #[test]
    fn resolve_bin_scans_path_left_to_right_before_well_known() {
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let want = exec_stub(first.path(), "op", 0o755);
        exec_stub(second.path(), "op", 0o755);
        let path =
            std::env::join_paths([Path::new("/nonexistent-dir"), first.path(), second.path()])
                .unwrap();
        let got = resolve_bin("op", None, Some(&path), &[Path::new("/bin/sh")]);
        assert_eq!(got.unwrap(), want);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_bin_skips_a_non_executable_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skipped = exec_stub(dir.path(), "op", 0o644);
        let err = resolve_bin("op", None, Some(dir.path().as_os_str()), &absent()).unwrap_err();
        assert_eq!(err[0], skipped.to_string_lossy());
        assert!(!is_executable_file(&skipped));
        assert!(!is_executable_file(dir.path()));
    }

    #[test]
    fn resolve_bin_falls_back_to_well_known_path() {
        let got = resolve_bin(
            "orca-definitely-absent",
            None,
            Some(std::ffi::OsStr::new("/usr/bin:/bin")),
            &[Path::new("/bin/sh")],
        );
        assert_eq!(got.unwrap(), Path::new("/bin/sh"));
    }

    #[test]
    fn resolve_bin_not_found_lists_every_path_tried() {
        let err = resolve_bin(
            "op",
            None,
            Some(std::ffi::OsStr::new("/nonexistent-a:/nonexistent-b")),
            &absent(),
        )
        .unwrap_err();
        let mut want = vec![
            "/nonexistent-a/op".to_string(),
            "/nonexistent-b/op".to_string(),
        ];
        want.extend(ABSENT.iter().map(|p| p.to_string()));
        assert_eq!(err, want);
    }

    #[test]
    fn resolve_bin_unset_path_tries_only_well_known() {
        let err = resolve_bin("op", None, None, &absent()).unwrap_err();
        assert_eq!(err, ABSENT.to_vec());
    }
}
