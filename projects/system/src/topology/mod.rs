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
    assign_claim_uuids(&mut out);
    out
}

/// Stamp each claim with its stable orca UUIDv7 (minted once, persisted in
/// `db::claim_identity`, keyed by the natural attributes). This host is the
/// source peer for the claims it collects, so it owns the mint and reports the
/// id on the wire. A DB failure leaves `uuid` empty — the inventory layer
/// guards, and the next tick retries — so it never blanks the snapshot.
fn assign_claim_uuids(claims: &mut [TopologyClaim]) {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "topology: claim-id db unavailable; ids deferred");
            return;
        }
    };
    for c in claims.iter_mut() {
        match db::claim_identity::resolve_or_mint(
            &conn,
            &c.provider,
            &c.provider_instance,
            &c.kind,
            &c.id,
        ) {
            Ok(uuid) => c.uuid = uuid,
            Err(e) => tracing::warn!(
                provider = %c.provider, kind = %c.kind, native_id = %c.id,
                error = %e, "topology: claim-id mint failed",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In a fresh test process there is no default DB, so
    /// `capability::is_available("proxmox")` returns false (the proxmox branch
    /// is skipped) and no external cdylib collectors are registered, so the
    /// registered-collector loop iterates nothing. `collect_claims` therefore
    /// walks both gates and returns an empty snapshot without touching any real
    /// provider — exercising the aggregation path deterministically.
    #[tokio::test]
    async fn collect_claims_empty_when_nothing_registered() {
        assert!(!crate::capability::is_available("proxmox"));
        assert!(contract::topology::collectors().is_empty());
        let claims = collect_claims().await;
        assert!(claims.is_empty());
    }
}
