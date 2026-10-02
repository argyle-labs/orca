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
        return is_executable(p).then(|| p.to_string_lossy().into_owned());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|c| is_executable(c))
        .map(|c| c.to_string_lossy().into_owned())
}

/// Is `p` a file this process could execute?
///
/// The mode check matters: a non-executable file with the right name is not a
/// command, and treating it as one turns a clear "not found" into a confusing
/// spawn failure further along.
#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
}
