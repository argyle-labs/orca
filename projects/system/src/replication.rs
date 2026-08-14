//! Pod-wide **replication relationship** definitions — the canonical,
//! defined-once source of truth for "this folder is kept in sync across these
//! member hosts by this provider", replicated across the fleet.
//!
//! A replication relationship is authored once and converges everywhere
//! ([[mesh-data-is-eventually-consistent]]): `lww = "updated_at"` opts the table
//! into eventually-consistent mesh sync, so every machine holds every
//! relationship definition. The macro owns the `updated_at` clock end to end.
//!
//! **`routes` IS the membership.** A relationship's member hosts are exactly the
//! built-in `routes` set every endpoint carries — the SAME generic primitive a
//! share uses for its ordered failover candidates ([[always-hunt-abstractions-reuse-seams]]).
//! There is deliberately no bespoke `members: [host]` list: willow and maple are
//! two `Route`s, index order is preference, and a route with `enabled = false` is
//! a held/drained member. Reusing `routes` means the resolver, dedup, and
//! self-annotation machinery a share already has apply to a relationship for free.
//!
//! Core is generic: `provider` and `folder` are **opaque** to core — the owning
//! backend plugin (`argyle-labs/syncthing`) interprets them. Core never links a
//! folder or measures sync itself; it only records the desired relationship and,
//! on read, asks the registered provider seam for observed `ReplicationStatus`
//! (`plugin_toolkit::storage::replication_status`). A share
//! references a relationship by its uuidv7 `id`
//! ([[reference-fields-are-nested-id-not-thingId]]-style ref); one relationship
//! can back the folder used by multiple shares.

use plugin_toolkit::endpoint_resource;

/// A replication relationship, defined once and replicated pod-wide. `name` (the
/// endpoint PK) is the fleet-unique role label — `media-replica` /
/// `backups-replica`. Member hosts are the built-in `routes`.
// `detail` is hand-written as `storage.replication.detail` (see `storage_tools.rs`):
// besides the config row it resolves the relationship's observed
// `ReplicationStatus` on read (host-local, on-demand), the read side of the
// config/health split. Skipping the macro `detail` keeps the tool name unique.
#[endpoint_resource(
    plugin = "storage.replication",
    table = "replications",
    lww = "updated_at",
    skip = "detail"
)]
pub struct Replication {
    /// Canonical uuidv7 identity ([[pure-uuidv7-ids-not-composite]]). A share
    /// references its replication relationship by this id; `name` is a
    /// descriptive, fleet-unique role label, not the identity.
    pub id: String,
    /// Backend that owns provisioning + status: `syncthing`. Descriptive —
    /// selects which plugin links the folder and reports sync health. Opaque to
    /// core.
    pub provider: String,
    /// The provider's folder identifier/label the members keep in sync (e.g. a
    /// Syncthing folder id). Opaque to core.
    pub folder: String,
}
