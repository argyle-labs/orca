//! Process-global allowlist of tools that paired pod peers may invoke via
//! `pod/exec`. Populated once at startup from `ToolRegistry::remote_ok_names`
//! so the pod listener can authorize without holding a registry handle.

use std::collections::HashSet;
use std::sync::OnceLock;

static REMOTE_OK: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Install the allowlist. Idempotent — first call wins; subsequent calls
/// are no-ops (matches the registry's single-instance lifecycle).
pub fn install(names: impl IntoIterator<Item = &'static str>) {
    let set: HashSet<&'static str> = names.into_iter().collect();
    let _ = REMOTE_OK.set(set);
}

/// True if a paired peer may invoke `tool` via `pod/exec`. Returns false
/// before `install` has been called.
pub fn is_allowed(tool: &str) -> bool {
    REMOTE_OK.get().map(|s| s.contains(tool)).unwrap_or(false)
}

/// Snapshot of currently-allowed names for introspection (`pod.exec` help).
pub fn snapshot() -> Vec<&'static str> {
    REMOTE_OK
        .get()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default()
}
