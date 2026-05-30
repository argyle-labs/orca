//! Process-global allowlist of tools that paired pod peers may invoke via
//! `pod/exec`. Populated once at startup from
//! `dispatch::remote_ok_names` so the pod listener can authorize
//! without walking the inventory on every request.

use std::collections::HashSet;
use std::sync::OnceLock;

static REMOTE_OK: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Install the allowlist. Idempotent — first call wins; subsequent calls
/// are no-ops (matches the registry's single-instance lifecycle).
pub fn install(names: impl IntoIterator<Item = &'static str>) {
    let set: HashSet<&'static str> = names.into_iter().collect();
    _ = REMOTE_OK.set(set);
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

#[cfg(test)]
mod tests {
    use super::*;

    // The allowlist is a process-wide OnceLock — all behavior lives in one
    // test so the uninstalled-state and first-call-wins assertions are
    // observable regardless of test-runner process model.
    #[test]
    fn install_lookup_snapshot_and_idempotency() {
        assert!(!is_allowed("system.detail"));
        assert!(snapshot().is_empty());

        install(["system.detail", "pod.list"]);
        assert!(is_allowed("system.detail"));
        assert!(is_allowed("pod.list"));
        assert!(!is_allowed("system.update"));
        let mut snap = snapshot();
        snap.sort();
        assert_eq!(snap, vec!["pod.list", "system.detail"]);

        // Subsequent install calls are no-ops — first call wins.
        install(["never_added"]);
        assert!(!is_allowed("never_added"));
    }
}
