//! Pod-wide NFS/SMB **share** definitions — the canonical, defined-once source
//! of truth for a network share, replicated across the fleet.
//!
//! A share is authored once and converges everywhere ([[mesh-data-is-eventually-consistent]]):
//! `lww = "updated_at"` opts the table into eventually-consistent mesh sync, so
//! every machine holds every share definition (drift becomes impossible, and the
//! fleet doubles as a distributed backup of its own config). The macro owns the
//! `updated_at` clock end to end — it is stamped on every write and never a tool
//! argument.
//!
//! Core is generic: `options` and `options_rendered` are **opaque** strings the
//! owning backend plugin (`argyle-labs/nfs`, `argyle-labs/smb`) produced. Core
//! never interprets them — the applier feeds `options_rendered` to `mount(8)`
//! verbatim. The typed, per-backend option surface lives in the plugins, so an
//! NFS caller never sees an SMB field and vice-versa.
//!
//! Ordered failover sources are the share's typed `routes` (built into every
//! endpoint): index 0 is the primary, the rest are failovers in priority order,
//! and a route with `enabled = false` is *held* (drained, excluded from
//! election). Each route folds a `host:/export` source to `value = host`,
//! `path = "/export"`, `port` defaulting per fstype.

use plugin_toolkit::endpoint_resource;

/// A network share, defined once and replicated pod-wide. `name` (the endpoint
/// PK) is the fleet-unique canonical role — `data` / `backups` / `downloads`.
// `list` is hand-written as `storage.share.list` (see `storage_tools.rs`): it
// dispatches on a `live` flag — default reads the replicated table, `live=true`
// enumerates shares straight off the registered backends. Skipping the macro's
// `list` keeps the mangled tool name unique.
// `update` is also hand-written as `storage.share.update` (see `storage_tools.rs`):
// besides the CRUD PATCH it dispatches coordinated source operations
// (`action=drain|resume|reboot_source`) that hold/return a failover route and
// orchestrate a source reboot. Skipping the macro `update` keeps the tool name
// unique.
#[endpoint_resource(
    plugin = "storage.share",
    table = "shares",
    lww = "updated_at",
    skip = "list,update"
)]
pub struct Share {
    /// Canonical uuidv7 identity ([[pure-uuidv7-ids-not-composite]]). A mount
    /// references its share by this id; `name` is a descriptive, fleet-unique
    /// role label, not the identity.
    pub id: String,
    /// Backend that owns rendering + validation: `nfs`, `smb`. Descriptive —
    /// selects which plugin interprets `options`.
    pub backend: String,
    /// Filesystem type passed to `mount -t` (`nfs4`, `cifs`).
    pub fstype: String,
    /// The owning plugin's typed option object as opaque JSON, kept for
    /// edit/round-trip. Core never parses it.
    pub options: String,
    /// The concrete `mount(8)` option string the plugin rendered from `options`
    /// at declare time — what the applier feeds `mount -o`. Opaque to core.
    pub options_rendered: String,
    /// Credential reference (a SecretRef the secrets domain resolves). Persisted,
    /// never surfaced.
    #[secret]
    pub credential: Option<String>,
    /// Optional reference to a replication relationship (by its uuidv7 `id`, see
    /// [`crate::replication`]) that keeps this share's route members in sync. When
    /// set, converge's failover-safety gate consults the relationship's observed
    /// `ReplicationStatus` (`plugin_toolkit::storage::replication_status`) and only
    /// fails the active route A→B over when replication between them is healthy —
    /// failing over to unreplicated/stale data is worse than holding. `None` ⇒ the
    /// share fails over freely (unchanged pre-gate behaviour).
    pub replication: Option<String>,
}
