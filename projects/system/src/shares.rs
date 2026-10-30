//! Mesh-wide NFS/SMB **share** definitions — the canonical, defined-once source
//! of truth for a network share, replicated across the fleet.
//!
//! A share is authored once and converges everywhere ([[mesh-data-is-eventually-consistent]]):
//! `lww = "updated_at"` opts the table into eventually-consistent mesh sync, so
//! every machine holds every share definition (drift becomes impossible, and the
//! fleet doubles as a distributed backup of its own config). The macro owns the
//! `updated_at` clock end to end — it is stamped on every write and never a tool
//! argument.
//!
//! Core is generic: `options` is the authored comma-separated mount option
//! string, opaque to core. The owning backend plugin (`argyle-labs/nfs`,
//! `argyle-labs/smb`) validates and renders it per mount at converge time, so
//! per-target details (an inline credential's `credentials=` path) are derived,
//! never stored. The typed, per-backend option surface lives in the plugins, so an
//! NFS caller never sees an SMB field and vice-versa.
//!
//! Ordered failover sources are the share's typed `routes` (built into every
//! endpoint): index 0 is the primary, the rest are failovers in priority order,
//! and a route with `enabled = false` is *held* (drained, excluded from
//! election). Each route folds a `host:/export` source to `value = host`,
//! `path = "/export"`, `port` defaulting per fstype.

use contract::SyncTargetSpec;
use plugin_toolkit::endpoint_resource;

/// A network share, defined once and replicated mesh-wide. `name` (the endpoint
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
    /// Authored comma-separated mount options (`vers=3.1.1,nosharesock`). The
    /// backend renders them per mount; core never parses them.
    pub options: String,
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
    /// JSON-encoded `Vec<SyncTargetSpec>` (the Sync seam, orca#339 slice 1) —
    /// opaque to the macro/DB layer like `options`. Consume via
    /// [`parse_sync_targets`] / [`encode_sync_targets`] — never parse ad hoc.
    pub sync_targets: String,
    /// Whether this share is served out to the fleet (rendered as the
    /// backend's export definition) rather than only mounted in.
    pub fleet_export: bool,
}

/// Parse a `Share`'s `sync_targets` column. The one place this JSON is
/// decoded for reads — malformed/legacy data yields an empty list rather than
/// failing the row read.
pub fn parse_sync_targets(raw: &str) -> Vec<SyncTargetSpec> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Encode targets for storage in the `sync_targets` column — the write-side
/// counterpart of [`parse_sync_targets`].
pub fn encode_sync_targets(targets: &[SyncTargetSpec]) -> String {
    serde_json::to_string(targets).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::{SyncTargetSpec, encode_sync_targets, parse_sync_targets};

    #[test]
    fn empty_string_parses_to_empty_list() {
        assert!(parse_sync_targets("").is_empty());
    }

    #[test]
    fn malformed_json_parses_to_empty_list_not_an_error() {
        assert!(parse_sync_targets("not json").is_empty());
    }

    #[test]
    fn round_trips_through_encode_and_parse() {
        let targets = vec![SyncTargetSpec {
            source: "/data/photos".to_string(),
            target: "nas:/backup/photos".to_string(),
            method: "syncthing".to_string(),
        }];
        let encoded = encode_sync_targets(&targets);
        assert_eq!(parse_sync_targets(&encoded), targets);
    }
}
