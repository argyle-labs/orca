//! Minimal-backup contract — how a managed unit declares its restore-sufficient
//! state, the policy that gates mutation on a prior backup, and the payload to
//! restore from one.
//!
//! See `docs/MINIMAL-BACKUP.md`. The guiding rule is **minimal = state, not
//! bulk**: a unit declares only the paths that are irreplaceable (app configs +
//! DBs, compose/stack definitions, unit definition), never media libraries,
//! caches, re-pullable images, or the reproducible OS. Everything here is pure,
//! typed declaration — the actual archive write is done by the `service` crate's
//! `BackupMethod` (tar/pbs), keeping this crate free of backup machinery deps.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a unit's minimal state is captured. A provider MAY compose several (e.g.
/// a VM returns both [`BackupStrategy::Definition`] and [`BackupStrategy::Paths`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupStrategy {
    /// Archive the declared [`BackupSpec::include`] paths. The minimal default —
    /// correct for service hosts and docker stacks where state lives in known
    /// config directories and bulk data lives elsewhere (network storage).
    Paths,
    /// Snapshot the whole rootfs. Correct ONLY when the rootfs *is* the state and
    /// is small (tiny containers); any bulk data must live on a separate mount
    /// that is excluded from the snapshot.
    Rootfs,
    /// The unit definition only (cores/mem/net/disk layout, compose file). Pairs
    /// with [`BackupStrategy::Paths`] for VMs — define the shell, archive the state.
    Definition,
}

/// A unit kind's declaration of its minimal, restore-sufficient state.
///
/// The generalization of the `service` crate's `ServiceBackend::data_paths()`
/// from the service domain to every managed unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackupSpec {
    /// Paths (in the unit's own filesystem namespace) that constitute state.
    /// Empty is valid for a pure [`BackupStrategy::Definition`] / [`BackupStrategy::Rootfs`] unit.
    #[serde(default)]
    pub include: Vec<String>,
    /// Sub-paths under `include` to exclude: caches, thumbnails, sockets, logs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// One or more strategies composed to capture this unit's state.
    pub strategies: Vec<BackupStrategy>,
}

impl BackupSpec {
    /// A paths-only minimal spec (the common service-host / stack case).
    pub fn paths(include: impl IntoIterator<Item = String>) -> Self {
        Self {
            include: include.into_iter().collect(),
            exclude: Vec::new(),
            strategies: vec![BackupStrategy::Paths],
        }
    }

    /// A tiny-container spec: the rootfs is the state.
    pub fn rootfs() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            strategies: vec![BackupStrategy::Rootfs],
        }
    }

    /// True if this spec captures nothing — a provider returning this is opting a
    /// kind out of guarded backups and must be treated as "cannot back up".
    pub fn is_empty(&self) -> bool {
        self.strategies.is_empty()
    }
}

/// Whether a mutating action must be preceded by a successful backup.
///
/// Default is [`BackupPolicy::Prompt`]: interactive callers are asked (default
/// yes), non-interactive callers back up automatically. In all backup-taking
/// cases a failed backup ABORTS the mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupPolicy {
    /// Ask interactively (default yes); auto-backup when non-interactive.
    #[default]
    Prompt,
    /// Always back up first, no opt-out.
    Always,
    /// Never back up automatically (the caller takes responsibility).
    Never,
}

/// A stable reference to a produced backup, used to restore.
///
/// Deliberately lighter than the `service` crate's `BackupArtifact` so this
/// crate stays free of backup-machinery deps; the two are reconciled when
/// backup/restore are folded onto the managed-unit surface (RFC increment 1b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackupRef {
    /// Storage-relative or absolute locator of the archive/snapshot.
    pub locator: String,
    /// Producing manager (e.g. `proxmox@cluster-a`), for routing a restore back.
    pub manager: String,
    /// Unix seconds when the backup completed.
    pub timestamp: i64,
    /// Optional integrity checksum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

/// Payload for a `Update { action: "restore" }` on a managed unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RestorePayload {
    /// Which backup to restore from.
    pub from: BackupRef,
    /// Optional single-component scope (e.g. restore just one service inside a
    /// multi-service host). `None` restores the whole unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_default_is_prompt() {
        assert_eq!(BackupPolicy::default(), BackupPolicy::Prompt);
    }

    #[test]
    fn paths_spec_is_not_empty_and_rootfs_helper_works() {
        let s = BackupSpec::paths(["/opt/appdata".to_string()]);
        assert!(!s.is_empty());
        assert_eq!(s.strategies, vec![BackupStrategy::Paths]);
        assert_eq!(
            BackupSpec::rootfs().strategies,
            vec![BackupStrategy::Rootfs]
        );
    }

    #[test]
    fn empty_spec_opts_out() {
        let s = BackupSpec {
            include: vec![],
            exclude: vec![],
            strategies: vec![],
        };
        assert!(s.is_empty());
    }

    #[test]
    fn restore_payload_roundtrips() {
        let p = RestorePayload {
            from: BackupRef {
                locator: "pbs:ct/100/2026".into(),
                manager: "proxmox@a".into(),
                timestamp: 1,
                checksum: None,
            },
            component: Some("sonarr".into()),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: RestorePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
