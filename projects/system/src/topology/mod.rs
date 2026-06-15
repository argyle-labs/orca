//! Topology claim collectors.
//!
//! A "claim" is "this host runs that child" — emitted by the colocated peer
//! (the one with the API/creds) and consumed by the inference task to derive
//! `parent_peer_id` edges via MAC matching. Per
//! [[project-colocated-api-collectors]], collectors run *only* on the peer
//! adjacent to the API endpoint; credentials never cross hosts.
//!
//! Slice A: docker + proxmox. Unraid lands next.

use contract::TopologyClaim;

mod proxmox;

/// Collect topology claims from every provider this host can reach locally.
/// Each provider is gated on the per-host capability registry — absent
/// providers are skipped silently so a host without docker doesn't log a
/// warning every tick. Operator can re-enable via `system.capability.recheck`
/// after installing the missing runtime.
///
/// A broken Available provider still logs (one broken collector must not
/// blank out the whole snapshot).
pub async fn collect_claims() -> Vec<TopologyClaim> {
    let mut out = Vec::new();
    if crate::capability::is_available("docker") {
        match docker::topology::collect_claims().await {
            Ok(mut v) => out.append(&mut v),
            Err(e) => tracing::warn!(error = %e, "topology: docker collector failed"),
        }
    }
    if crate::capability::is_available("proxmox") {
        match proxmox::collect_all().await {
            Ok(mut v) => out.append(&mut v),
            Err(e) => tracing::warn!(error = %e, "topology: proxmox collector failed"),
        }
    }
    out
}
