//! Cross-crate topology types.
//!
//! `TopologyClaim` is emitted by colocated provider plugins (proxmox,
//! unraid, docker, ...) describing "this host runs that child" and consumed
//! by the system crate's inference task to derive parent_peer_id edges via
//! MAC matching. Lives here (not in `system`) so plugins can produce claims
//! without depending on `system`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One child entity a host claims to run. The inference layer matches each
/// claim's `macs` against other peers' `interfaces[].mac` to derive
/// `parent_peer_id`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct TopologyClaim {
    /// `"vm"`, `"container"`, `"lxc"`.
    pub kind: String,
    /// Provider-native id (proxmox vmid, docker container id short, ...).
    pub id: String,
    pub name: String,
    /// MAC addresses associated with this child (lowercase, colon-separated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Provider that emitted this claim (`"proxmox"`, `"docker"`,
    /// `"unraid"`, ...).
    pub provider: String,
    /// Provider instance id. For docker = `"local"`; for proxmox = the
    /// endpoint name from `db::proxmox`; for secret-keyed providers = the
    /// `<instance>` segment of `<provider>.<instance>.<field>`.
    pub provider_instance: String,
}
