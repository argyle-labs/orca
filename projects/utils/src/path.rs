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
