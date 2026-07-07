//! Multi-channel dial-target selection (Slice 5 of host_addressing_plan).
//!
//! Given the local host's addressing channels and a peer's known addresses,
//! produce an ordered list of dial targets to try. Pure function — no I/O,
//! no DB. Callers (`pod::ping`, `pod-scheduler`, `pod-bootstrap`) handle the
//! actual socket attempts with their own timeout / fallback policy.
//!
//! Preference order (per the plan):
//!   1. `lan_v4`        if peer has one AND we share a /24 (cheap subnet match)
//!   2. `tailscale_v4`  if both sides have Tailscale
//!   3. `fqdn`          if peer has one (DNS does its own routing)
//!   4. `lan_v6`        if peer has one and we have any v6
//!   5. `tailscale_v6`  if both sides have Tailscale v6
//!   6. legacy single `peer_addr` from `pod_peers` (rc.≤24 fallback)
//!
//! Returning `Vec<String>` rather than a single pick keeps the policy simple
//! while letting callers retry the rest of the list on connect failure.

/// Channel-kind constants — match the `host_addressing.key` / `pod_peer_addresses.kind`
/// vocabulary used in the DB.
pub const LAN_V4: &str = "lan_v4";
pub const LAN_V6: &str = "lan_v6";
pub const TAILSCALE_V4: &str = "tailscale_v4";
pub const TAILSCALE_V6: &str = "tailscale_v6";
pub const FQDN: &str = "fqdn";

/// One (kind, value) row — caller flattens its DB rows into this shape so the
/// dialer doesn't depend on `orca-db` types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub kind: String,
    pub value: String,
}

impl Channel {
    pub fn new(kind: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            value: value.into(),
        }
    }
}

/// Produce an ordered list of dial targets for the peer, given the local
/// host's channels and the peer's channels. `legacy_peer_addr` is the
/// single-address fallback from `pod_peers.peer_addr` (used for rc.≤24 peers
/// that don't propagate a snapshot yet); if non-empty it's appended last.
///
/// Duplicates are filtered: if the legacy address already appears as a
/// channel value, it's not re-added.
pub fn select_dial_targets(
    local: &[Channel],
    peer: &[Channel],
    legacy_peer_addr: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // 1. lan_v4 — only if we share a /24
    if let Some(peer_v4) = first_value(peer, LAN_V4)
        && local
            .iter()
            .filter(|c| c.kind == LAN_V4)
            .any(|c| same_v4_slash_24(&c.value, peer_v4))
    {
        push_unique(&mut out, peer_v4);
    }

    // 2. tailscale_v4 — only if BOTH sides have Tailscale
    if let (Some(peer_ts4), Some(_)) = (
        first_value(peer, TAILSCALE_V4),
        first_value(local, TAILSCALE_V4),
    ) {
        push_unique(&mut out, peer_ts4);
    }

    // 3. fqdn — DNS routes itself; no local-side gate
    if let Some(peer_fqdn) = first_value(peer, FQDN) {
        push_unique(&mut out, peer_fqdn);
    }

    // 4. lan_v6 — peer must have one and we must have any v6 ourselves
    if let Some(peer_v6) = first_value(peer, LAN_V6)
        && local
            .iter()
            .any(|c| c.kind == LAN_V6 || c.kind == TAILSCALE_V6)
    {
        push_unique(&mut out, peer_v6);
    }

    // 5. tailscale_v6 — gated like tailscale_v4
    if let (Some(peer_ts6), Some(_)) = (
        first_value(peer, TAILSCALE_V6),
        first_value(local, TAILSCALE_V6),
    ) {
        push_unique(&mut out, peer_ts6);
    }

    // 6. legacy fallback
    if !legacy_peer_addr.is_empty() {
        push_unique(&mut out, legacy_peer_addr);
    }

    out
}

fn first_value<'a>(rows: &'a [Channel], kind: &str) -> Option<&'a str> {
    rows.iter()
        .find(|c| c.kind == kind)
        .map(|c| c.value.as_str())
}

fn push_unique(out: &mut Vec<String>, v: &str) {
    if !out.iter().any(|x| x == v) {
        out.push(v.to_string());
    }
}

/// Cheap "same private LAN" gate — compare first three octets. Returns false
/// on any parse error so we never mis-route to a non-routable address.
fn same_v4_slash_24(a: &str, b: &str) -> bool {
    let prefix = |s: &str| -> Option<[u8; 3]> {
        let mut it = s.split('.');
        let a = it.next()?.parse::<u8>().ok()?;
        let b = it.next()?.parse::<u8>().ok()?;
        let c = it.next()?.parse::<u8>().ok()?;
        let _d = it.next()?.parse::<u8>().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some([a, b, c])
    };
    matches!((prefix(a), prefix(b)), (Some(x), Some(y)) if x == y)
}

/// Try `f(target)` for each `target` in order. Return the first `Ok` value;
/// if every attempt fails, return the last error. `Err("no dial targets")`
/// when the slice is empty.
///
/// Generic over the future + return so this combinator is testable without
/// touching the TLS stack. Callers compose this with `call_typed` (or any
/// per-target dial function) to get full multi-channel retry semantics.
pub async fn try_targets<R, F, Fut>(targets: &[String], mut f: F) -> anyhow::Result<R>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<R>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for t in targets {
        match f(t.clone()).await {
            Ok(r) => return Ok(r),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no dial targets")))
}

/// DB-backed convenience wrapper: load this host's `host_addressing` rows
/// and the named peer's `pod_peer_addresses` rows, then return the dial-target
/// list. `legacy_peer_addr` is the single-address fallback from
/// `pod_peers.peer_addr` for rc.≤24 compat.
pub fn dial_targets_for_peer(
    conn: &rusqlite::Connection,
    peer_id: &str,
    legacy_peer_addr: &str,
) -> anyhow::Result<Vec<String>> {
    let local: Vec<Channel> = db::host_addressing::list_host_addressing(conn)?
        .into_iter()
        .map(|r| Channel::new(r.key, r.value))
        .collect();
    let peer: Vec<Channel> = db::host_addressing::list_peer_addresses(conn, peer_id)?
        .into_iter()
        .map(|r| Channel::new(r.kind, r.value))
        .collect();
    Ok(select_dial_targets(&local, &peer, legacy_peer_addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(kind: &str, value: &str) -> Channel {
        Channel::new(kind, value)
    }

    #[test]
    fn empty_inputs_produce_empty_output() {
        assert!(select_dial_targets(&[], &[], "").is_empty());
    }

    #[test]
    fn legacy_addr_used_when_no_channels() {
        let out = select_dial_targets(&[], &[], "10.0.0.5");
        assert_eq!(out, vec!["10.0.0.5"]);
    }

    #[test]
    fn lan_v4_preferred_when_same_subnet() {
        let local = vec![ch(LAN_V4, "10.0.0.4")];
        let peer = vec![ch(LAN_V4, "10.0.0.5"), ch(FQDN, "host-g.lan")];
        let out = select_dial_targets(&local, &peer, "");
        assert_eq!(out, vec!["10.0.0.5", "host-g.lan"]);
    }

    #[test]
    fn lan_v4_skipped_when_different_subnet() {
        let local = vec![ch(LAN_V4, "10.0.0.4")];
        let peer = vec![ch(LAN_V4, "192.168.1.5"), ch(FQDN, "host-g.lan")];
        let out = select_dial_targets(&local, &peer, "");
        // lan_v4 fails /24 match → fqdn moves up; lan_v4 not retried
        assert_eq!(out, vec!["host-g.lan"]);
    }

    #[test]
    fn tailscale_requires_both_sides() {
        let peer = vec![ch(TAILSCALE_V4, "100.64.1.2")];
        // Local has no tailscale → skip
        assert!(select_dial_targets(&[], &peer, "").is_empty());
        // Both have it → pick
        let local = vec![ch(TAILSCALE_V4, "100.64.0.9")];
        let out = select_dial_targets(&local, &peer, "");
        assert_eq!(out, vec!["100.64.1.2"]);
    }

    #[test]
    fn full_preference_order() {
        let local = vec![
            ch(LAN_V4, "10.0.0.4"),
            ch(TAILSCALE_V4, "100.64.0.9"),
            ch(LAN_V6, "fe80::1"),
            ch(TAILSCALE_V6, "fd7a::1"),
        ];
        let peer = vec![
            ch(FQDN, "host-g.example.test"),
            ch(LAN_V6, "fe80::5"),
            ch(TAILSCALE_V4, "100.64.1.2"),
            ch(LAN_V4, "10.0.0.5"),
            ch(TAILSCALE_V6, "fd7a::5"),
        ];
        let out = select_dial_targets(&local, &peer, "10.0.0.5");
        assert_eq!(
            out,
            vec![
                "10.0.0.5",            // 1. lan_v4 (same /24)
                "100.64.1.2",          // 2. tailscale_v4
                "host-g.example.test", // 3. fqdn
                "fe80::5",             // 4. lan_v6
                "fd7a::5",             // 5. tailscale_v6
                                       // 6. legacy "10.0.0.5" dedup'd
            ]
        );
    }

    #[test]
    fn legacy_addr_dedup_against_channel() {
        let local = vec![ch(LAN_V4, "10.0.0.4")];
        let peer = vec![ch(LAN_V4, "10.0.0.5")];
        let out = select_dial_targets(&local, &peer, "10.0.0.5");
        assert_eq!(out, vec!["10.0.0.5"]);
    }

    #[test]
    fn lan_v6_requires_local_v6_of_any_kind() {
        let peer = vec![ch(LAN_V6, "fe80::5")];
        // No local v6 → skip
        assert!(select_dial_targets(&[], &peer, "").is_empty());
        // tailscale_v6 counts as "we have v6"
        let local = vec![ch(TAILSCALE_V6, "fd7a::1")];
        let out = select_dial_targets(&local, &peer, "");
        assert_eq!(out, vec!["fe80::5"]);
    }

    #[tokio::test]
    async fn try_targets_returns_first_success() {
        let targets = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut tried: Vec<String> = Vec::new();
        let out = try_targets(&targets, |t| {
            tried.push(t.clone());
            async move {
                if t == "b" {
                    Ok::<_, anyhow::Error>(42)
                } else {
                    Err(anyhow::anyhow!("nope: {t}"))
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(out, 42);
        // stops after first success
        assert_eq!(tried, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn try_targets_returns_last_error_on_all_fail() {
        let targets = vec!["a".to_string(), "b".to_string()];
        let err = try_targets(&targets, |t| async move {
            Err::<(), _>(anyhow::anyhow!("fail: {t}"))
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("fail: b"), "got: {err}");
    }

    #[tokio::test]
    async fn try_targets_empty_slice_errors() {
        let err = try_targets::<(), _, _>(&[], |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no dial targets"));
    }

    #[test]
    fn malformed_v4_does_not_match_subnet() {
        // Defense in depth — bad input never satisfies same_v4_slash_24.
        assert!(!same_v4_slash_24("not.an.ip", "10.0.0.5"));
        assert!(!same_v4_slash_24("10.0.0.5", ""));
        assert!(!same_v4_slash_24("10.0.0.5.6", "10.0.0.5"));
        assert!(same_v4_slash_24("10.0.0.4", "10.0.0.250"));
        assert!(!same_v4_slash_24("10.0.1.4", "10.0.0.250"));
    }
}
