//! Process-global DENYLIST of tools that paired pod peers may NOT invoke via
//! `pod/exec`. Everything is REMOTE_OK by default; only `local_only` tools
//! (`#[orca_tool(local_only = true)]`) are refused. Populated once at startup
//! from `dispatch::local_only_names` so the pod listener can authorize without
//! walking the inventory on every request.
//!
//! This is the reachability axis. Authorization (which *role* a caller needs)
//! is the separate, orthogonal `tool_roles` layer.

use std::collections::HashSet;
use std::sync::OnceLock;

static LOCAL_ONLY: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Install the denylist. Idempotent — first call wins; subsequent calls are
/// no-ops (matches the registry's single-instance lifecycle). Pass the
/// `local_only` tool names (the opt-outs), NOT the allowed ones.
pub fn install(local_only: impl IntoIterator<Item = &'static str>) {
    let set: HashSet<&'static str> = local_only.into_iter().collect();
    _ = LOCAL_ONLY.set(set);
}

/// True if a paired peer may invoke `tool` via `pod/exec`. Default is ALLOW:
/// a tool is reachable unless it is on the `local_only` denylist. Before
/// `install` runs (startup, before the listener binds) everything reads as
/// allowed — the denylist simply isn't known yet.
pub fn is_allowed(tool: &str) -> bool {
    !LOCAL_ONLY.get().map(|s| s.contains(tool)).unwrap_or(false)
}

/// Snapshot of the currently-denied (`local_only`) names for introspection.
pub fn snapshot_local_only() -> Vec<&'static str> {
    LOCAL_ONLY
        .get()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The denylist is a process-wide OnceLock — all behavior lives in one test
    // so the uninstalled-state and first-call-wins assertions are observable
    // regardless of test-runner process model.
    #[test]
    fn default_allow_with_local_only_denylist() {
        // Before install: everything is allowed (default REMOTE_OK).
        assert!(is_allowed("system.detail"));
        assert!(is_allowed("proxmox.cluster_list"));
        assert!(snapshot_local_only().is_empty());

        // Install a denylist of local_only opt-outs.
        install(["system.dev_enable", "system.dev_disable"]);

        // Denylisted tools are refused; everything else — core, plugin,
        // never-seen — stays allowed by default.
        assert!(!is_allowed("system.dev_enable"));
        assert!(!is_allowed("system.dev_disable"));
        assert!(is_allowed("system.detail"));
        assert!(is_allowed("proxmox.cluster_list"));
        assert!(is_allowed("some.brand_new.plugin_tool"));

        let mut snap = snapshot_local_only();
        snap.sort();
        assert_eq!(snap, vec!["system.dev_disable", "system.dev_enable"]);

        // First call wins — later installs are no-ops.
        install(["never_added"]);
        assert!(is_allowed("never_added"));
    }
}
