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

/// Locate an executable on `$PATH` via the system `which` command.
/// Returns the resolved absolute path, or `None` if not found.
/// Callers needing only an existence check can use `which(name).is_some()`.
pub fn which(name: &str) -> Option<String> {
    let out = std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_replaces_leading_tilde_with_home() {
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
}
