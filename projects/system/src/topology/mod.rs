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
    // Registered topology collectors contributed by loaded cdylib plugins
    // (proxmox, unraid, …) through the loader's `topology` domain. Each runs on
    // ANY host that has the plugin's creds — e.g. the API-based Proxmox
    // collector walks every registered + enabled endpoint, so bravo gets
    // nested under delta from hotel or foxtrot too. A collector that errors is
    // logged and skipped so one broken provider can't blank the snapshot. This
    // is the external-plugin load path that replaces the old in-tree
    // `::proxmox` / unraid static calls.
    //
    // docker's collector likewise arrives through this loop as an external
    // cdylib, so there is no in-tree `docker::topology::collect_claims()` call.
    //
    // These run BEFORE the in-core pmxcfs fallback so the API path is
    // authoritative: on a PVE host that also has a registered Proxmox endpoint,
    // the plugin already emits one claim per guest (`provider = "proxmox"`).
    // The in-core file-reader would emit a SECOND claim per guest with a
    // different `provider_instance` ("local" vs the endpoint name), which the
    // inventory dedup keys on — so both survive and every guest doubles. We
    // therefore run the pmxcfs collector only as a fallback when no plugin
    // contributed proxmox claims (bare PVE host with no API endpoint).
    for collector in contract::topology::collectors() {
        match collector.collect_claims().await {
            Ok(mut v) => out.append(&mut v),
            // A registered collector for a runtime that isn't present here (e.g.
            // docker on a host with no docker socket) fails every ~2s. Throttle
            // per (provider, error) — once, then at most once per 5 min. The
            // collector still runs each tick; only the log is gated.
            Err(e) => {
                let key = format!("topology:collect:{}:{e}", collector.name());
                if plugin_toolkit::logging::should_warn_throttled(
                    &key,
                    std::time::Duration::from_secs(300),
                ) {
                    tracing::warn!(
                        provider = %collector.name(),
                        error = %e,
                        "topology: plugin collector failed",
                    );
                }
            }
        }
    }

    // In-core pmxcfs fallback. Fires only when (a) this host is a Proxmox host
    // (capability probed from `/etc/pve` / `pveversion`) AND (b) no loaded
    // plugin already produced proxmox claims via the API path above. This keeps
    // guest coverage on a bare PVE host with no registered endpoint while
    // avoiding the double-count when an endpoint IS registered.
    if should_run_incore_proxmox(&out, crate::capability::is_available("proxmox")) {
        match proxmox::collect_all().await {
            Ok(mut v) => out.append(&mut v),
            Err(e) => tracing::warn!(error = %e, "topology: proxmox collector failed"),
        }
    }

    assign_claim_uuids(&mut out);
    out
}

/// Decide whether the in-core pmxcfs proxmox fallback should run.
///
/// It runs only when this host is Proxmox-capable AND no loaded plugin already
/// contributed proxmox claims through the API path. On a PVE host that also has
/// a registered Proxmox endpoint the plugin emits one claim per guest, so the
/// pmxcfs reader must stay quiet — otherwise each guest surfaces twice (the two
/// claims carry different `provider_instance` values, "local" vs the endpoint
/// name, and the inventory dedup keys on that, so both survive). With no plugin
/// coverage (bare PVE host, no endpoint) the fallback fires and preserves guest
/// topology.
fn should_run_incore_proxmox(existing: &[TopologyClaim], proxmox_capable: bool) -> bool {
    proxmox_capable && !existing.iter().any(|c| c.provider == "proxmox")
}

/// Stamp each claim with its stable orca UUIDv7 (minted once, persisted in
/// `db::claim_identity`, keyed by the natural attributes). This host is the
/// source peer for the claims it collects, so it owns the mint and reports the
/// id on the wire. A DB failure leaves `uuid` empty — the inventory layer
/// guards, and the next tick retries — so it never blanks the snapshot.
fn assign_claim_uuids(claims: &mut [TopologyClaim]) {
    // Reuse the server's pooled connection. Opening a fresh SQLCipher
    // connection here on every topology tick (~15s) leaked memory on
    // Proxmox hosts — each open pays PBKDF2 + a large page-cache alloc +
    // OpenSSL key setup. The pool wins in the daemon; CLI/tests fall back
    // to a single fresh open per process.
    let res = db::pool::with_pooled_or_open(|conn| {
        for c in claims.iter_mut() {
            match db::claim_identity::resolve_or_mint(
                conn,
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
        Ok(())
    });
    if let Err(e) = res {
        tracing::warn!(error = %e, "topology: claim-id db unavailable; ids deferred");
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

    fn proxmox_claim(id: &str, provider_instance: &str) -> TopologyClaim {
        TopologyClaim {
            kind: "vm".to_string(),
            id: id.to_string(),
            name: format!("guest-{id}"),
            macs: vec!["02:00:00:00:00:01".to_string()],
            provider: "proxmox".to_string(),
            provider_instance: provider_instance.to_string(),
            ..Default::default()
        }
    }

    /// Overlap scenario: a PVE host that also has a registered Proxmox endpoint.
    /// The plugin (API) collector has already contributed one claim for guest
    /// 100, so the in-core pmxcfs fallback must NOT run — otherwise the same
    /// guest would be claimed a second time with `provider_instance = "local"`
    /// and the inventory dedup (keyed on provider/instance/kind/native-id) would
    /// keep both, doubling the node. Exactly one claim must remain.
    #[test]
    fn incore_fallback_suppressed_when_plugin_covers_proxmox() {
        let out = vec![proxmox_claim("100", "delta")];
        assert!(!should_run_incore_proxmox(&out, true));
        // Nothing new is appended, so the single guest yields exactly one claim.
        assert_eq!(out.iter().filter(|c| c.id == "100").count(), 1);
    }

    /// Bare PVE host, no registered endpoint: the plugin contributed nothing, so
    /// the pmxcfs fallback MUST run to preserve guest coverage.
    #[test]
    fn incore_fallback_runs_on_bare_pve_without_endpoint() {
        assert!(should_run_incore_proxmox(&[], true));
    }

    /// A non-Proxmox host never runs the pmxcfs reader regardless of claims.
    #[test]
    fn incore_fallback_never_runs_when_not_proxmox_capable() {
        assert!(!should_run_incore_proxmox(&[], false));
        assert!(!should_run_incore_proxmox(
            &[proxmox_claim("1", "x")],
            false
        ));
    }
}
