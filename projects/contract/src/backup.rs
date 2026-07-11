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

/// When a unit's scheduled backups run.
///
/// `Cron` carries a full 5-field expression for anything the named cadences
/// don't cover; the named variants are conveniences the scheduler maps to a
/// canonical cron.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupSchedule {
    /// No scheduled backups — on demand and pre-mutation only.
    Manual,
    Hourly,
    #[default]
    Daily,
    Weekly,
    Monthly,
    /// Full 5-field cron expression (e.g. `"35 3 * * *"`).
    Cron(String),
}

/// How many backups to keep, mirroring the PBS / vzdump `prune-backups` model.
/// Every field is independent; `None` means "unbounded on this axis". At least
/// one bound should be set or backups grow forever.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct Retention {
    /// Keep the N most recent regardless of age (e.g. `keep_last = 5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_last: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_hourly: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_daily: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_weekly: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_monthly: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_yearly: Option<u32>,
}

impl Default for Retention {
    /// Sensible default: keep the last 7.
    fn default() -> Self {
        Self {
            keep_last: Some(7),
            keep_hourly: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
        }
    }
}

impl Retention {
    /// A `keep-last N` retention.
    pub fn keep_last(n: u32) -> Self {
        Self {
            keep_last: Some(n),
            ..Self::default()
        }
    }

    /// True if no axis is bounded — backups would grow forever.
    pub fn is_unbounded(&self) -> bool {
        self.keep_last.is_none()
            && self.keep_hourly.is_none()
            && self.keep_daily.is_none()
            && self.keep_weekly.is_none()
            && self.keep_monthly.is_none()
            && self.keep_yearly.is_none()
    }
}

/// Whether a mutating action must be preceded by a successful backup. Distinct
/// from the *schedule* — this gates on-change protection, not the cadence.
///
/// Default [`BackupGate::Prompt`]: interactive callers are asked (default yes),
/// non-interactive callers back up automatically. When a backup is taken, its
/// failure ABORTS the mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupGate {
    /// Ask interactively (default yes); auto-backup when non-interactive.
    #[default]
    Prompt,
    /// Always back up first, no opt-out.
    Always,
    /// Never back up automatically (the caller takes responsibility).
    Never,
}

impl BackupGate {
    /// Resolve whether a pre-mutation backup should run.
    ///
    /// - [`BackupGate::Always`] → `Some(true)` (unconditional).
    /// - [`BackupGate::Never`] → `Some(false)` (unconditional).
    /// - [`BackupGate::Prompt`] → `None` when `interactive` (the caller must ask
    ///   the user, default yes); `Some(true)` otherwise (non-interactive callers
    ///   back up automatically).
    ///
    /// Keeps prompting and policy storage out of the contract layer: a caller
    /// maps `None` to its own yes/no prompt, then feeds the answer to the guard.
    pub fn decide(&self, interactive: bool) -> Option<bool> {
        match self {
            BackupGate::Always => Some(true),
            BackupGate::Never => Some(false),
            BackupGate::Prompt if interactive => None,
            BackupGate::Prompt => Some(true),
        }
    }
}

/// A unit's complete backup policy: when scheduled backups run, how many are
/// kept, whether mutations are gated on a backup, and an optional method hint.
/// Deliberately a struct (not an enum) so the "lots of backup settings" can grow
/// additively without breaking callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackupPolicy {
    /// Whether scheduled backups are active at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cadence for scheduled backups.
    #[serde(default)]
    pub schedule: BackupSchedule,
    /// Prune/retention rules.
    #[serde(default)]
    pub retention: Retention,
    /// Pre-mutation protection.
    #[serde(default)]
    pub gate: BackupGate,
    /// Preferred [`crate::backup`] write method (`"tar"` / `"pbs"`); `None` =
    /// auto-select (the `service` crate's `select_method`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for BackupPolicy {
    /// Daily, keep-last-7, prompt-gated, auto method — the safe fleet default.
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: BackupSchedule::Daily,
            retention: Retention::default(),
            gate: BackupGate::Prompt,
            method: None,
        }
    }
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

/// How a [`BackupTarget`]'s directory is backed.
///
/// orca resolves the target against its storage layer: for a network backing it
/// reuses an existing mount at the target path or provisions one (nfs/smb/s3);
/// [`BackupBacking::Local`] is a plain directory on the host with no mount — the
/// always-available fallback when no network storage is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BackupBacking {
    /// A plain directory on the host filesystem (no mount).
    Local,
    /// An NFS export.
    Nfs { server: String, export: String },
    /// An SMB/CIFS share.
    Smb { server: String, share: String },
    /// An S3 (or S3-compatible) bucket.
    S3 {
        bucket: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// Non-AWS endpoint for S3-compatible stores (MinIO, Backblaze, …).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },
}

/// Where a system's backups are written. Every orca system has (or can be given)
/// exactly one primary target; backups may also be assigned to any folder ad hoc.
///
/// Resolution (delegated to the storage domain): if `path` is not already a
/// mountpoint and `backing` is a network kind, orca reuses a matching existing
/// mount or provisions a new one; if `backing` is [`BackupBacking::Local`] it
/// just ensures the directory exists. This lets a system always have a usable
/// backups dir even with no network storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackupTarget {
    /// Absolute path that receives backups (a mountpoint or a plain local dir).
    pub path: String,
    /// How `path` is backed.
    pub backing: BackupBacking,
    /// If true and the mount/dir is missing, orca provisions it; if false it
    /// errors rather than creating storage.
    #[serde(default = "default_true")]
    pub provision_if_missing: bool,
}

impl BackupTarget {
    /// A plain local-directory target — the no-network fallback.
    pub fn local(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            backing: BackupBacking::Local,
            provision_if_missing: true,
        }
    }

    /// True if this target needs a network mount (vs a plain local dir).
    pub fn needs_mount(&self) -> bool {
        !matches!(self.backing, BackupBacking::Local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_default_is_daily_keep7_prompt() {
        let p = BackupPolicy::default();
        assert!(p.enabled);
        assert_eq!(p.schedule, BackupSchedule::Daily);
        assert_eq!(p.retention.keep_last, Some(7));
        assert_eq!(p.gate, BackupGate::Prompt);
        assert!(!p.retention.is_unbounded());
    }

    #[test]
    fn unbounded_retention_is_flagged() {
        assert!(
            Retention {
                keep_last: None,
                ..Retention::default()
            }
            .keep_last
            .is_none()
        );
        let unbounded = Retention {
            keep_last: None,
            keep_hourly: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
        };
        assert!(unbounded.is_unbounded());
        assert!(!Retention::keep_last(5).is_unbounded());
    }

    #[test]
    fn cron_schedule_roundtrips() {
        let s = BackupSchedule::Cron("35 3 * * *".into());
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<BackupSchedule>(&j).unwrap(), s);
    }

    #[test]
    fn local_target_needs_no_mount_network_does() {
        assert!(!BackupTarget::local("/var/backups").needs_mount());
        let nfs = BackupTarget {
            path: "/mnt/backups".into(),
            backing: BackupBacking::Nfs {
                server: "10.0.0.1".into(),
                export: "/export/backups".into(),
            },
            provision_if_missing: true,
        };
        assert!(nfs.needs_mount());
        let j = serde_json::to_string(&nfs).unwrap();
        assert_eq!(serde_json::from_str::<BackupTarget>(&j).unwrap(), nfs);
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

    #[test]
    fn gate_decision_resolves_per_mode() {
        // Unconditional modes ignore interactivity.
        assert_eq!(BackupGate::Always.decide(true), Some(true));
        assert_eq!(BackupGate::Always.decide(false), Some(true));
        assert_eq!(BackupGate::Never.decide(true), Some(false));
        assert_eq!(BackupGate::Never.decide(false), Some(false));
        // Prompt: ask when interactive, default-yes when not.
        assert_eq!(BackupGate::Prompt.decide(true), None);
        assert_eq!(BackupGate::Prompt.decide(false), Some(true));
    }
}
