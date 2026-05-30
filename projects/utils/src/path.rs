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
