//! Transitional compatibility with pre-rename peers.
//!
//! # This module is the ONLY place the old name may appear
//!
//! The mesh used to be called a "pod". That concept is gone from the codebase —
//! the crate, the modules, the types, the tables, the verbs, the operator
//! strings. What cannot be deleted by editing this repository is the name
//! already baked into daemons that are *running elsewhere*: a peer on a
//! pre-rename build answers only the old TLS SNI and speaks only the old
//! subscribe frames, and no amount of renaming here changes that.
//!
//! So every remaining occurrence lives here, behind named predicates, as a
//! single unit with a single deletion trigger. Nothing outside this module
//! spells the old name; callers ask questions like
//! [`accepts_server_sni`] instead.
//!
//! # Why a shim and not a clean break
//!
//! A clean break was tried and it is what broke the fleet. #651 flipped the
//! dialer to the new SNI while leaving listeners answering both. That is not
//! compatibility — it is a one-directional bridge: an old host could reach a
//! new one, a new host could never reach an old one. Measured against thor on
//! 2026-09-27 with `openssl s_client`: the legacy name completed the
//! handshake, the current name returned `tlsv1 alert access denied`. The fleet
//! split in half and the roll that was supposed to converge it had to travel
//! over the direction that was broken.
//!
//! The lesson is the ordering rule this module exists to enforce: **a receiver
//! is upgraded before a sender that depends on it.** Accept both names; keep
//! emitting the name every peer already understands; flip emission only once
//! nothing old remains.
//!
//! # Deletion trigger
//!
//! Delete this module, its call sites, and the legacy SANs in
//! [`crate::pki::mesh_server_sans`] once every system in the mesh runs
//! `0.2.1-rc.7` or newer. As of 2026-09-27 that is 8 of 10 hosts; bragi and
//! hemlock are powered-off gaming boxes still on rc.6, and they are the reason
//! this exists. Verify with `orca system health` before deleting — a host that
//! is merely unreachable is not a host that has been upgraded.

/// SNI the paired-peer mTLS surface answered on before the rename.
const LEGACY_SERVER_SNI: &str = "pod.orca.local";

/// SNI the pre-pairing bootstrap surface answered on before the rename.
const LEGACY_BOOTSTRAP_SNI: &str = "pod-bootstrap.orca.local";

/// Subscribe event frame name used by pre-rename peers.
pub const LEGACY_SUBSCRIBE_EVENT: &str = "pod/subscribe.event";

/// Subscribe heartbeat frame name used by pre-rename peers.
pub const LEGACY_SUBSCRIBE_HEARTBEAT: &str = "pod/subscribe.heartbeat";

/// Should the paired-peer listener answer this SNI?
///
/// True for the current name and for the pre-rename one, so a peer that has
/// not been upgraded yet can still reach this host.
///
/// Gated with `pki`, which owns the CURRENT names: a consumer thin enough to
/// build `utils` without PKI has no listener to answer with.
#[cfg(feature = "pki")]
pub fn accepts_server_sni(sni: &str) -> bool {
    sni == crate::pki::MESH_SERVER_SAN || sni == LEGACY_SERVER_SNI
}

/// Should the bootstrap listener answer this SNI? See [`accepts_server_sni`].
#[cfg(feature = "pki")]
pub fn accepts_bootstrap_sni(sni: &str) -> bool {
    sni == crate::pki::MESH_BOOTSTRAP_SAN || sni == LEGACY_BOOTSTRAP_SNI
}

/// The SNI to retry a dial with when the current name is refused.
///
/// This is the half #651 never had, and the reason an upgraded controller
/// could not reach a pre-rename host at all: the dialer sent the current name,
/// the old listener rejected it, and the failure looked like a broken peer
/// rather than a name mismatch. A caller dials with
/// [`crate::pki::MESH_SERVER_SAN`] first and falls back to this only when the
/// handshake is refused, so an upgraded fleet never pays for it.
pub fn server_sni_fallback() -> &'static str {
    LEGACY_SERVER_SNI
}

/// Legacy DNS names a mesh SERVER cert must also carry so a pre-rename peer
/// can validate it. Empty once this module is deleted.
pub fn legacy_server_sans() -> Vec<String> {
    vec![LEGACY_SERVER_SNI.to_string()]
}

/// Legacy per-host DNS names a mesh CLIENT cert must also carry.
pub fn legacy_client_sans(host_cn: &str) -> Vec<String> {
    vec![format!("{host_cn}.{LEGACY_SERVER_SNI}")]
}

/// Legacy DNS names a BOOTSTRAP cert must also carry.
pub fn legacy_bootstrap_sans() -> Vec<String> {
    vec![LEGACY_BOOTSTRAP_SNI.to_string()]
}

/// Does `err` mean "this peer cannot be reached under the CURRENT name",
/// rather than the connection failing outright?
///
/// Two distinct shapes, both measured on this fleet on 2026-09-27:
///
/// 1. **The peer refuses the name.** A pre-rename listener rejects the current
///    SNI at the TLS layer. rustls renders the alert as one word
///    (`AccessDenied`), OpenSSL as two (`access denied`) — matching only the
///    spaced form would make this predicate silently never fire.
///
/// 2. **The peer's CERT does not carry the name.** The likelier case, and the
///    one that actually took the fleet down: every host was upgraded, but their
///    server certs were issued before the rename and carry only the legacy SAN:
///
///    ```text
///    invalid peer certificate: certificate not valid for name
///    "mesh.orca.local"; certificate is only valid for DnsName("pod.orca.local")
///    ```
///
///    A mesh cert lives 30 days and the reissue path has to reach a signing peer
///    OVER THE MESH — so the cert that cannot be validated is the same cert
///    needed to fetch its replacement. That deadlock does not clear on its own;
///    dialing the legacy name breaks it.
///
/// Anything else — a host that is down, a genuine CA mismatch — must NOT retry:
/// a second dial would double the timeout on an unreachable host.
pub fn is_sni_refusal(err: &str) -> bool {
    let e = err.to_lowercase();
    let refused_the_name = e.contains("accessdenied")
        || e.contains("access denied")
        || e.contains("unrecognized name")
        || e.contains("unrecognized_name")
        || e.contains("handshake failure");
    // "not valid for name" is the cert half. Pinned to the name mismatch, not to
    // certificate errors generally: an UnknownIssuer is a real CA mismatch and
    // retrying under another name would only hide it.
    let cert_lacks_the_name = e.contains("not valid for name");
    refused_the_name || cert_lacks_the_name
}

#[cfg(all(test, feature = "pki"))]
mod tests {
    use super::*;

    #[test]
    fn the_listener_answers_both_names() {
        assert!(accepts_server_sni(crate::pki::MESH_SERVER_SAN));
        assert!(accepts_server_sni("pod.orca.local"));
        assert!(accepts_bootstrap_sni(crate::pki::MESH_BOOTSTRAP_SAN));
        assert!(accepts_bootstrap_sni("pod-bootstrap.orca.local"));
        // An unrelated name is still refused — accepting both is not accepting any.
        assert!(!accepts_server_sni("example.com"));
        assert!(!accepts_bootstrap_sni("example.com"));
        // The two surfaces stay distinct; one must never answer the other's name.
        assert!(!accepts_server_sni(crate::pki::MESH_BOOTSTRAP_SAN));
        assert!(!accepts_bootstrap_sni(crate::pki::MESH_SERVER_SAN));
    }

    #[test]
    fn only_a_name_refusal_earns_a_second_dial() {
        assert!(is_sni_refusal("received fatal alert: AccessDenied"));
        assert!(is_sni_refusal("tlsv1 alert access denied"));
        // The measured fleet-wide failure, verbatim from mint on 2026-09-27:
        // every host upgraded, every server cert still pre-rename.
        assert!(is_sni_refusal(
            "TLS handshake (is the peer's mesh CA the same as ours?): invalid peer \
             certificate: certificate not valid for name \"mesh.orca.local\"; \
             certificate is only valid for DnsName(\"pod.orca.local\")"
        ));
        // A down host, or a real CA mismatch, must not be dialed twice.
        assert!(!is_sni_refusal(
            "connect 10.0.0.1:12002: Operation timed out"
        ));
        assert!(!is_sni_refusal("No route to host"));
        assert!(!is_sni_refusal("invalid peer certificate: UnknownIssuer"));
    }

    #[test]
    fn certs_carry_the_legacy_names_so_old_peers_can_validate_them() {
        // Issuance must stay dual-named for as long as the listener answers
        // both: a cert carrying only the current name would be rejected by the
        // very peers this module exists to keep reachable.
        assert!(legacy_server_sans().contains(&"pod.orca.local".to_string()));
        assert!(legacy_bootstrap_sans().contains(&"pod-bootstrap.orca.local".to_string()));
        assert!(legacy_client_sans("host-a").contains(&"host-a.pod.orca.local".to_string()));
    }
}
