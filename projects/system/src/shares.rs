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
//! Core is generic: `sources`, `options`, and `options_rendered` are **opaque**
//! strings the owning backend plugin (`argyle-labs/nfs`, `argyle-labs/smb`)
//! produced. Core never interprets them — the applier feeds `options_rendered`
//! to `mount(8)` verbatim. The typed, per-backend option surface lives in the
//! plugins, so an NFS caller never sees an SMB field and vice-versa.

use plugin_toolkit::endpoint_resource;

/// A network share, defined once and replicated pod-wide. `name` (the endpoint
/// PK) is the fleet-unique canonical role — `data` / `backups` / `downloads`.
#[endpoint_resource(plugin = "storage_share", table = "shares", lww = "updated_at")]
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
    /// Ordered sources as a JSON array of `host:/export` strings — index 0 is the
    /// primary, the rest are failovers in priority order. Opaque to core; the
    /// applier elects one at mount time.
    pub sources: String,
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
}
