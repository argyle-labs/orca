//! Proxmox → TopologyClaim collector (stub).
//!
//! Runs on the colocated peer with Proxmox API creds. Emits one claim per
//! VM/LXC with its MAC(s) so the inference task can derive `parent_peer_id`
//! edges. Implementation pending — collector wiring + API client land next.

use crate::system_info_types::TopologyClaim;

pub async fn collect_all() -> anyhow::Result<Vec<TopologyClaim>> {
    Ok(Vec::new())
}
