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
    // docker's collector now arrives through the loader's `topology` domain as
    // an external cdylib (picked up by the registered-collector loop below), so
    // there is no in-tree `docker::topology::collect_claims()` static call.
    if crate::capability::is_available("proxmox") {
        match proxmox::collect_all().await {
            Ok(mut v) => out.append(&mut v),
            Err(e) => tracing::warn!(error = %e, "topology: proxmox collector failed"),
        }
    }
    // Registered topology collectors contributed by loaded cdylib plugins
    // (proxmox, unraid, …) through the loader's `topology` domain. Each runs on
    // ANY host that has the plugin's creds — e.g. the API-based Proxmox
    // collector walks every registered + enabled endpoint, so bravo gets
    // nested under delta from hotel or foxtrot too. A collector that errors is
    // logged and skipped so one broken provider can't blank the snapshot. This
    // is the external-plugin load path that replaces the old in-tree
    // `::proxmox` / unraid static calls.
    for collector in contract::topology::collectors() {
        match collector.collect_claims().await {
            Ok(mut v) => out.append(&mut v),
            Err(e) => tracing::warn!(
                provider = %collector.name(),
                error = %e,
                "topology: plugin collector failed",
            ),
        }
    }
    out
}
