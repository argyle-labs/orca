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
    /// correct for service hosts and stacks where state lives in known config
    /// directories and bulk data lives elsewhere (network storage).
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
/// don't cover; the named variants map to a canonical cron via [`to_cron`]:
/// `Hourly` at minute 0, `Daily`/`Weekly`/`Monthly` at 04:00, `Weekly` on Sunday,
/// `Monthly` on the 1st. The clock is the schedule's resolved timezone
/// ([`BackupPolicy::timezone`]).
///
/// [`to_cron`]: BackupSchedule::to_cron
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupSchedule {
    /// No scheduled backups — on demand and pre-mutation only.
    Manual,
    Hourly,
    Daily,
    /// The default cadence: Sunday at 04:00.
    #[default]
    Weekly,
    Monthly,
    /// Full 5-field cron expression (e.g. `"35 3 * * *"`).
    Cron(String),
}

impl BackupSchedule {
    /// The 5-field cron (`min hour dom mon dow`) this cadence runs on, evaluated
    /// in the schedule's resolved timezone. `Manual` has no cron and returns
    /// `None`; `Cron` returns its own expression verbatim.
    pub fn to_cron(&self) -> Option<String> {
        let expr = match self {
            BackupSchedule::Manual => return None,
            BackupSchedule::Hourly => "0 * * * *",
            BackupSchedule::Daily => "0 4 * * *",
            BackupSchedule::Weekly => "0 4 * * 0",
            BackupSchedule::Monthly => "0 4 1 * *",
            BackupSchedule::Cron(c) => return Some(c.clone()),
        };
        Some(expr.to_string())
    }
}

/// One gibibyte, the default [`Retention::max_total_bytes`] cap.
pub const ONE_GIB: u64 = 1_073_741_824;

/// How many backups to keep, mirroring the PBS / vzdump `prune-backups` model
/// plus a total-size cap. Every field is independent; `None` means "unbounded on
/// this axis". The count axes keep the newest that satisfy each; `max_total_bytes`
/// then prunes oldest-first until the collection fits. At least one bound should
/// be set or backups grow forever.
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
    /// Cap on the total on-disk size of this collection; oldest backups are
    /// pruned until the sum fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_bytes: Option<u64>,
}

impl Default for Retention {
    /// The default: keep the last 25 backups, capped at 1 GiB total.
    fn default() -> Self {
        Self {
            keep_last: Some(25),
            keep_hourly: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            max_total_bytes: Some(ONE_GIB),
        }
    }
}

impl Retention {
    /// A `keep-last N` retention with no size cap.
    pub fn keep_last(n: u32) -> Self {
        Self {
            keep_last: Some(n),
            keep_hourly: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            max_total_bytes: None,
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
            && self.max_total_bytes.is_none()
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

/// Where a resolved schedule/retention value came from. A surface tells the user
/// it is "using the default" only for [`PolicySource::Default`]; a value set at
/// the backup or storage level is shown plainly even when it equals the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    /// The backup's own policy (the per-target binding) set this value.
    Backup,
    /// The storage/target default set this value.
    Storage,
    /// Neither was set; this is the built-in default.
    Default,
}

impl PolicySource {
    /// True when the value is the built-in default — the only case a surface
    /// annotates as "using the default".
    pub fn is_default(&self) -> bool {
        matches!(self, PolicySource::Default)
    }
}

/// A value together with the policy tier it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub source: PolicySource,
}

/// Resolve retention across the tiers, most specific first: the backup's own
/// policy, then the storage default, then the built-in [`Retention::default`].
pub fn resolve_retention(
    backup: Option<Retention>,
    storage: Option<Retention>,
) -> Resolved<Retention> {
    match (backup, storage) {
        (Some(value), _) => Resolved {
            value,
            source: PolicySource::Backup,
        },
        (None, Some(value)) => Resolved {
            value,
            source: PolicySource::Storage,
        },
        (None, None) => Resolved {
            value: Retention::default(),
            source: PolicySource::Default,
        },
    }
}

/// Resolve a schedule across the tiers, most specific first: the backup's own
/// policy, then the storage default, then the built-in [`BackupSchedule::default`].
pub fn resolve_schedule(
    backup: Option<BackupSchedule>,
    storage: Option<BackupSchedule>,
) -> Resolved<BackupSchedule> {
    match (backup, storage) {
        (Some(value), _) => Resolved {
            value,
            source: PolicySource::Backup,
        },
        (None, Some(value)) => Resolved {
            value,
            source: PolicySource::Storage,
        },
        (None, None) => Resolved {
            value: BackupSchedule::default(),
            source: PolicySource::Default,
        },
    }
}

/// A unit's complete backup policy, stored in the unit's metadata: whether
/// backups are active, pre-mutation gating, an optional method hint, the schedule
/// timezone, and the list of targets backups fan out to. Each target binding
/// carries its own schedule/retention; resolution falls back to the storage
/// default then the built-in default ([`resolve_retention`] / [`resolve_schedule`]).
/// Deliberately a struct (not an enum) so the settings can grow additively
/// without breaking callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackupPolicy {
    /// Whether scheduled backups are active at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Pre-mutation protection.
    #[serde(default)]
    pub gate: BackupGate,
    /// Preferred [`crate::backup`] write method (`"tar"` / `"pbs"`); `None` =
    /// auto-select (the `service` crate's `select_method`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// IANA timezone the schedule's clock runs in (e.g. `"America/Chicago"`).
    /// `None` inherits the fleet-wide default from the `backup`/`timezone` config
    /// row; absent that, the system local time. Set per unit/service to override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Where backups are written — a list of independent target bindings, each a
    /// destination the backup fans out to with its OWN schedule and retention
    /// ([[no-top-level-urls-use-addresses-array]],
    /// [[backup-target-independent-retention-schedule]]). Editable after creation.
    /// Empty resolves to the built-in `local` target.
    #[serde(default)]
    pub targets: Vec<BackupTargetBinding>,
}

fn default_true() -> bool {
    true
}

impl Default for BackupPolicy {
    /// Enabled, prompt-gated, auto method, inherited timezone, no targets — the
    /// safe fleet default. Schedule/retention resolve per-target down to the
    /// built-in default (weekly Sunday 04:00; keep 25 or 1 GiB).
    fn default() -> Self {
        Self {
            enabled: true,
            gate: BackupGate::Prompt,
            method: None,
            timezone: None,
            targets: Vec::new(),
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

/// A produced, listable backup — the unit a restore selects by date.
///
/// Carries the identity a caller needs to *list* a kind's backups and *pick one
/// by date*, which the `backup.*` tool surface returns. Timestamps are Unix
/// **milliseconds** ([[time-values-in-milliseconds]]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupRecord {
    /// Stable, sortable id for this backup: the compact UTC stamp
    /// (`YYYYMMDD-HHMMSS`). Unique within a `(kind, instance)` and the value a
    /// restore selects with. Sorting ids lexically sorts them chronologically.
    pub id: String,
    /// The backup KIND / provider that produced it (`host`, `service`, `nfs`, …).
    pub kind: String,
    /// Instance within the kind. `default` when the kind is single-instance.
    pub instance: String,
    /// When the backup completed, Unix milliseconds.
    pub created_ms: i64,
    /// Absolute path to this backup's payload directory on the host that holds it.
    pub path: String,
    /// Total payload size in bytes.
    #[serde(default)]
    pub size_bytes: u64,
    /// Number of files captured in the payload.
    #[serde(default)]
    pub file_count: u64,
    /// Optional integrity checksum over the payload (provider-defined algorithm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Free-form provider note (e.g. which strategy/paths were captured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl BackupRecord {
    /// The UTC calendar date (`YYYY-MM-DD`) this backup was taken, derived from
    /// the sortable [`id`](Self::id) stamp. Falls back to the empty string if the
    /// id is not in the expected `YYYYMMDD-HHMMSS` shape.
    pub fn date(&self) -> String {
        // id = "YYYYMMDD-HHMMSS"; slice the date half.
        self.id
            .split_once('-')
            .map(|(ymd, _)| {
                if ymd.len() == 8 {
                    format!("{}-{}-{}", &ymd[0..4], &ymd[4..6], &ymd[6..8])
                } else {
                    String::new()
                }
            })
            .unwrap_or_default()
    }
}

/// Which backup a restore targets within a `(kind, instance)`.
///
/// Kept deliberately small: a caller either names an explicit backup `id` (the
/// date-selected restore that MCP/REST require) or asks for the most recent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupSelector {
    /// The most recent backup for the instance.
    Latest,
    /// The backup whose [`BackupRecord::id`] equals this value.
    Id(String),
}

impl BackupSelector {
    /// Parse a caller-supplied selector string. `""`, `"latest"` (any case) →
    /// [`BackupSelector::Latest`]; anything else is treated as an explicit id.
    pub fn parse(s: &str) -> Self {
        let t = s.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("latest") {
            BackupSelector::Latest
        } else {
            BackupSelector::Id(t.to_string())
        }
    }
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

/// A reference to a configured backup TARGET — the WHERE axis, orthogonal to the
/// provider KIND (the WHAT). A target is named by its `kind` (a target kind in
/// the target-provider registry) plus a `name` that disambiguates multiple
/// targets of the same kind. The target kind's plugin owns the target's typed
/// settings; this ref names the kind and instance, and the target provider
/// resolves it to concrete storage
/// ([[orca-core-generic-plugins-expose-functionality]], [[no-kind-owned-by-plugin]]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupTargetRef {
    /// Target KIND — the registry key (`local`, or a plugin's `nfs`/`s3`/…).
    pub kind: String,
    /// Instance name within the kind. `default` for the single, unnamed target.
    #[serde(default = "default_target_name")]
    pub name: String,
}

fn default_target_name() -> String {
    "default".to_string()
}

impl BackupTargetRef {
    /// The built-in `local`/`default` target — the always-available fallback.
    pub fn local() -> Self {
        Self {
            kind: "local".to_string(),
            name: default_target_name(),
        }
    }

    /// A named target of the given kind.
    pub fn new(kind: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
        }
    }

    /// True if this is the built-in local file-path target.
    pub fn is_local(&self) -> bool {
        self.kind == "local"
    }
}

impl Default for BackupTargetRef {
    fn default() -> Self {
        Self::local()
    }
}

/// A target bound into a policy with its OWN schedule and retention. Each binding
/// is independent: one target keeps 7 daily, another 4 weekly, a third 12
/// monthly, each on its own cadence, and pruning one never touches another's
/// payloads ([[backup-target-independent-retention-schedule]]). `schedule` and
/// `retention` are per-target overrides; absent, the binding inherits the
/// policy-level defaults (see [`BackupPolicy::schedule_for`] /
/// [`BackupPolicy::retention_for`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupTargetBinding {
    /// The target this binding writes to.
    #[serde(flatten)]
    pub target: BackupTargetRef,
    /// Per-target schedule override; `None` inherits the policy default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<BackupSchedule>,
    /// Per-target retention override; `None` inherits the policy default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
}

impl BackupTargetBinding {
    /// A binding on the given target that inherits the policy's schedule and
    /// retention.
    pub fn inherit(target: BackupTargetRef) -> Self {
        Self {
            target,
            schedule: None,
            retention: None,
        }
    }

    /// A binding on the given target with its own schedule and retention.
    pub fn new(target: BackupTargetRef, schedule: BackupSchedule, retention: Retention) -> Self {
        Self {
            target,
            schedule: Some(schedule),
            retention: Some(retention),
        }
    }
}

/// A set of opaque placement labels describing where a workload runs, consulted
/// by a target's `fits()` to decide whether to offer that target
/// ([[topology-must-model-guest-services]]). It informs which targets are
/// offered; it never gates a user's explicit choice.
///
/// A plugin that manages a platform assigns the label it owns (e.g. the Proxmox
/// plugin tags a host `"proxmox"`), and that plugin's target `fits()` is the code
/// that interprets it. Labels are meaningful only to the `fits()` that checks for
/// them ([[orca-core-generic-plugins-expose-functionality]]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    /// The host the workload runs on, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Opaque placement labels, plugin-assigned (e.g. `"proxmox"`). Core does not
    /// interpret them; a target's `fits()` checks for the labels it understands.
    #[serde(default)]
    pub labels: Vec<String>,
}

impl Placement {
    /// A placement with no labels — the conservative default (fits everything the
    /// caller doesn't restrict).
    pub fn bare() -> Self {
        Self::default()
    }

    /// A placement carrying the given labels.
    pub fn with_labels(labels: impl IntoIterator<Item = String>) -> Self {
        Self {
            host: None,
            labels: labels.into_iter().collect(),
        }
    }

    /// True if this placement carries `label`.
    pub fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// Provider-supplied metadata about a completed backup, folded into the
/// [`BackupRecord`] the store writes.
///
/// Lives in `contract` (not the `system` daemon crate) so an out-of-process
/// backup-KIND plugin can name and return it across the JSON-proxy boundary —
/// a plugin cannot depend on `system`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupOutcome {
    /// Optional integrity checksum over the payload (provider-defined algorithm).
    pub checksum: Option<String>,
    /// Free-form note on what was captured (paths, strategy, …).
    pub note: Option<String>,
}

/// A concrete storage location a target kind exposes for selection — the "point
/// a target" surface. A storage plugin (smb/nfs) enumerates the mounts/shares it
/// manages; the backup-create flow lists these so the user picks the ROOT (e.g.
/// the smb `/backups` mount). The sub-path beneath is the provider-declared
/// taxonomy by default.
///
/// Lives in `contract` so an out-of-process backup-TARGET plugin can return it
/// across the JSON-proxy boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetLocation {
    /// Stable id within the kind — becomes the target ref `name` when selected.
    pub id: String,
    /// Human label for the picker (e.g. `SMB //nas/backups`).
    pub label: String,
    /// The base filesystem path this location roots at, when it is a mounted /
    /// local path. Absent for object stores addressed by key (s3) — those carry
    /// the address in [`backing_key`](Self::backing_key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_path: Option<String>,
    /// GLOBALLY STABLE identity of the underlying storage, used for FLEET-WIDE
    /// collision detection: two hosts collide only when they write the same
    /// `backing_key` + overlapping sub-path. Per-host local disks are namespaced
    /// (`local://<host>`) so they never collide cross-host; shared backings carry
    /// their shared address (`nfs://server/export`, `s3://bucket`).
    pub backing_key: String,
}

/// The out-of-process backup KIND/TARGET wire protocol: the op names and the
/// arg/reply envelopes exchanged as JSON across the plugin proxy boundary.
///
/// Both sides encode/decode the same types from here — the host proxy
/// (`system::backup::proxy`) and the plugin-side dispatch helper
/// (`plugin_toolkit::backup`) — so the contract is defined once and cannot
/// drift. Every arg/reply carries owned fields (it crosses a process boundary
/// as JSON) and both `Serialize` + `Deserialize` (each side does one direction).
pub mod wire {
    use super::{Deserialize, Placement, Serialize};

    /// Domain a plugin declares to contribute a backup KIND.
    pub const DOMAIN_KIND: &str = "backup_kind";
    /// Domain a plugin declares to contribute a backup TARGET.
    pub const DOMAIN_TARGET: &str = "backup_target";

    // KIND ops.
    pub const OP_INSTANCES: &str = "instances";
    pub const OP_LAYOUT: &str = "layout";
    pub const OP_BACKUP: &str = "backup";
    pub const OP_RESTORE: &str = "restore";
    // TARGET ops.
    pub const OP_OPEN: &str = "open";
    pub const OP_SYNC: &str = "sync";
    pub const OP_REFRESH: &str = "refresh";
    pub const OP_FITS: &str = "fits";
    pub const OP_DEFAULT_RETENTION: &str = "default_retention";
    pub const OP_DEFAULT_SCHEDULE: &str = "default_schedule";
    pub const OP_AVAILABLE: &str = "available";
    pub const OP_BACKING_KEY: &str = "backing_key";
    /// Human-facing title op — both KIND and TARGET expose it so the proxy can
    /// surface a plugin-supplied title over the wire (bridge Gap #4).
    pub const OP_TITLE: &str = "title";

    /// Args for the `layout` op — the instance whose layout segments are wanted.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct InstanceArgs {
        pub instance: String,
    }

    /// Args for `backup` / `restore` — the host-local payload dir the plugin
    /// subprocess reads/writes directly (shared filesystem; bytes never cross
    /// the wire) plus the instance being captured/restored.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PayloadArgs {
        pub payload_dir: String,
        pub instance: String,
    }

    /// Args for the target ops keyed by instance name (`open`/`sync`/`refresh`/
    /// `default_retention`/`default_schedule`/`backing_key`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct NameArgs {
        pub name: String,
    }

    /// Args for the `fits` op — the placement a target is asked to fit.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct FitsArgs {
        pub placement: Placement,
    }

    /// Reply from the `open` op — the host-local root path the plugin
    /// provisioned for this target instance. The generic store owns everything
    /// beneath it.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct OpenReply {
        pub root: String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_default_is_prompt_gated_with_no_targets() {
        let p = BackupPolicy::default();
        assert!(p.enabled);
        assert_eq!(p.gate, BackupGate::Prompt);
        assert!(p.timezone.is_none());
        assert!(p.targets.is_empty());
    }

    #[test]
    fn default_retention_is_keep25_or_1gib() {
        let r = Retention::default();
        assert_eq!(r.keep_last, Some(25));
        assert_eq!(r.max_total_bytes, Some(ONE_GIB));
        assert!(!r.is_unbounded());
    }

    #[test]
    fn weekly_default_cron_is_sunday_0400() {
        assert_eq!(BackupSchedule::default(), BackupSchedule::Weekly);
        assert_eq!(
            BackupSchedule::Weekly.to_cron().as_deref(),
            Some("0 4 * * 0")
        );
        assert_eq!(
            BackupSchedule::Daily.to_cron().as_deref(),
            Some("0 4 * * *")
        );
        assert_eq!(BackupSchedule::Manual.to_cron(), None);
        assert_eq!(
            BackupSchedule::Cron("5 1 * * *".into())
                .to_cron()
                .as_deref(),
            Some("5 1 * * *")
        );
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
            max_total_bytes: None,
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
    fn target_ref_defaults_to_local_and_round_trips() {
        let d = BackupTargetRef::default();
        assert!(d.is_local());
        assert_eq!(d, BackupTargetRef::local());
        assert_eq!(d.name, "default");

        // A bare `{"kind":"example-remote"}` fills name=default; camelCase on wire.
        let r: BackupTargetRef = serde_json::from_str(r#"{"kind":"example-remote"}"#).unwrap();
        assert_eq!(r, BackupTargetRef::new("example-remote", "default"));
        assert!(!r.is_local());

        let full = BackupTargetRef::new("example-remote", "cold");
        let j = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<BackupTargetRef>(&j).unwrap(), full);
    }

    #[test]
    fn policy_targets_default_empty_and_round_trip() {
        // Absent `targets` deserializes to empty (→ caller uses built-in local).
        let p: BackupPolicy = serde_json::from_str("{}").unwrap();
        assert!(p.targets.is_empty());

        // Generic non-core kind names — core never blesses specific plugin target
        // kinds (nfs/smb/s3 are plugins). One binding inherits policy defaults,
        // the other overrides both schedule and retention.
        let p = BackupPolicy {
            targets: vec![
                BackupTargetBinding::inherit(BackupTargetRef::local()),
                BackupTargetBinding::new(
                    BackupTargetRef::new("example-remote", "cold"),
                    BackupSchedule::Weekly,
                    Retention::keep_last(4),
                ),
            ],
            ..BackupPolicy::default()
        };
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<BackupPolicy>(&j).unwrap(), p);
    }

    #[test]
    fn retention_and_schedule_resolve_backup_then_storage_then_default() {
        // Neither backup nor storage set → built-in default, flagged as default.
        let r = resolve_retention(None, None);
        assert_eq!(r.value, Retention::default());
        assert_eq!(r.source, PolicySource::Default);
        assert!(r.source.is_default());
        let s = resolve_schedule(None, None);
        assert_eq!(s.value, BackupSchedule::Weekly);
        assert_eq!(s.source, PolicySource::Default);

        // Storage default fills in when the backup sets none → sourced to storage.
        let r = resolve_retention(None, Some(Retention::keep_last(3)));
        assert_eq!(r.value, Retention::keep_last(3));
        assert_eq!(r.source, PolicySource::Storage);
        assert!(!r.source.is_default());

        // A value set at the backup wins over storage, even equal to the default.
        let r = resolve_retention(Some(Retention::default()), Some(Retention::keep_last(3)));
        assert_eq!(r.value, Retention::default());
        assert_eq!(r.source, PolicySource::Backup);
        assert!(
            !r.source.is_default(),
            "set value is never shown as default"
        );
    }

    #[test]
    fn placement_labels_are_opaque() {
        // Labels are opaque; core assigns no meaning. A plugin's target `fits()`
        // is what checks for a label like this one (used here only as test data).
        assert!(!Placement::bare().has("proxmox"));
        let p = Placement::with_labels(["proxmox".to_string()]);
        assert!(p.has("proxmox"));
        assert!(!p.has("bare"));
        let j = serde_json::to_string(&p).unwrap();
        assert!(
            serde_json::from_str::<Placement>(&j)
                .unwrap()
                .has("proxmox")
        );
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

    fn sample_record() -> BackupRecord {
        BackupRecord {
            id: "20260731-041500".into(),
            kind: "host".into(),
            instance: "default".into(),
            created_ms: 1_785_000_000_000,
            path: "/var/backups/host/default/20260731-041500".into(),
            size_bytes: 4096,
            file_count: 3,
            checksum: Some("sha256:deadbeef".into()),
            note: Some("paths: ~/.claude/memory".into()),
        }
    }

    #[test]
    fn record_round_trips_and_is_camel_case() {
        let r = sample_record();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["id"], "20260731-041500");
        assert_eq!(v["createdMs"], 1_785_000_000_000i64);
        assert_eq!(v["sizeBytes"], 4096);
        assert_eq!(v["fileCount"], 3);
        let back: BackupRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn record_optional_fields_default() {
        // Only the required fields present; size/count default to 0, opts to None.
        let r: BackupRecord = serde_json::from_str(
            r#"{"id":"20260101-000000","kind":"d","instance":"i",
                "createdMs":1,"path":"/p"}"#,
        )
        .unwrap();
        assert_eq!(r.size_bytes, 0);
        assert_eq!(r.file_count, 0);
        assert!(r.checksum.is_none());
        assert!(r.note.is_none());
    }

    #[test]
    fn record_date_derives_from_id() {
        assert_eq!(sample_record().date(), "2026-07-31");
        // Malformed id → empty, never a panic.
        let mut bad = sample_record();
        bad.id = "nope".into();
        assert_eq!(bad.date(), "");
    }

    #[test]
    fn ids_sort_chronologically() {
        // The compact stamp is lexically == chronologically ordered.
        let mut ids = ["20260731-041500", "20260101-235959", "20260731-000001"];
        ids.sort();
        assert_eq!(
            ids,
            ["20260101-235959", "20260731-000001", "20260731-041500"]
        );
    }

    #[test]
    fn selector_parses_latest_and_id() {
        assert_eq!(BackupSelector::parse(""), BackupSelector::Latest);
        assert_eq!(BackupSelector::parse("  "), BackupSelector::Latest);
        assert_eq!(BackupSelector::parse("LATEST"), BackupSelector::Latest);
        assert_eq!(BackupSelector::parse("latest"), BackupSelector::Latest);
        assert_eq!(
            BackupSelector::parse("20260731-041500"),
            BackupSelector::Id("20260731-041500".into())
        );
    }

    #[test]
    fn selector_round_trips() {
        for s in [BackupSelector::Latest, BackupSelector::Id("x".into())] {
            let j = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<BackupSelector>(&j).unwrap(), s);
        }
    }
}
