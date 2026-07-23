//! Per-host **mount placements** — "host X mounts share Y at target Z" —
//! replicated pod-wide so the whole fleet's mount topology is visible and any
//! node can author a placement for any host ([[mesh-data-is-eventually-consistent]]).
//! Each host's convergence loop materializes only the rows whose `host` is its
//! own peer id.
//!
//! Supersedes the per-host-local `managed_mounts` table (which, being local and
//! unreplicated, is exactly why the fleet drifted). Named `mount` while the two
//! coexist; `managed_mounts` + autofs are retired once the convergence loop owns
//! materialization.
//!
//! A placement holds NO copy of the share's sources/options — it references the
//! share by `share_id`, so a host cannot drift a share it only points at.
//! `lww = "updated_at"` opts it into eventually-consistent mesh sync; the macro
//! owns the clock.

use plugin_toolkit::endpoint_resource;

/// A desired mount placement. The endpoint PK `name` IS the uuidv7 identity
/// ([[pure-uuidv7-ids-not-composite]]) — a placement has no human name — so the
/// primary key is a uuidv7 by construction.
#[endpoint_resource(plugin = "mount", table = "mounts", lww = "updated_at")]
pub struct Mount {
    /// The share this placement mounts, by its uuidv7 `shares.id`. Holds no copy
    /// of the share's sources/options — resolved at materialization time.
    pub share_id: String,
    /// The peer id of the host this placement targets. A host's convergence loop
    /// acts only on rows whose `host` equals its own peer id.
    pub host: String,
    /// Absolute mountpoint on `host`.
    pub target: String,
    /// Serialized remount policy (per-placement host behaviour, distinct from the
    /// share). Optional until the policy engine lands.
    pub remount_policy: Option<String>,
}
