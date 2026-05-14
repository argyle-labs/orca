//! Pod / mesh tools surfaced to the four-surface registry.
//!
//! `pod.list` mirrors the CLI's `orca pod list` so the web overview can
//! render paired peers without a bespoke REST endpoint. The remaining ops
//! delegate to `PodService` (registered by the server) because they need
//! mTLS dials, PKI material, and bootstrap signing — all server-side state
//! that this wasm-safe crate must not touch directly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Args / Output types (wasm-safe, shared by every surface) ────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPeerAddressDto {
    pub kind: String,
    pub value: String,
    pub source: String,
    pub last_seen_at: i64,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPeerDto {
    pub peer_id: String,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    pub last_seen_at: i64,
    pub local_secure: bool,
    pub peer_secure: bool,
    /// "active" | "departed".
    pub status: String,
    /// Multi-channel addresses (LAN v4/v6, Tailscale, FQDN, …). May be empty
    /// for peers paired before slice 4 of the host-addressing plan landed.
    #[serde(default)]
    pub addresses: Vec<PodPeerAddressDto>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodPeerList(pub Vec<PodPeerDto>);

// ── pod.accept ───────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodAcceptArgs {
    /// 6-char pairing code shown on the inviter's screen.
    pub code: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodAcceptOutput {
    pub pod_id: String,
    pub inviter_peer_id: String,
    pub inviter_hostname: String,
    pub inviter_addr: String,
    pub inviter_port: u16,
    /// `self_secure` flag after accept. Always false at this point — operator
    /// flips it on after verifying the join.
    pub self_secure: bool,
}

// ── pod.trust ────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodTrustArgs {
    pub peer_id: String,
    pub on: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodTrustOutput {
    pub peer_id: String,
    pub local_secure: bool,
    pub peer_secure: bool,
    pub mutual: bool,
    pub notify_result: String,
}

// ── pod.ping ─────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPingArgs {
    /// Paired peer ID (`peer.<machine_id_short>`) — looked up in `pod_peers`
    /// for the dial target.
    pub peer_id: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPingOutput {
    pub ok: bool,
    pub latency_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

// ── pod.discover ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDiscoveryRowDto {
    pub pubkey_fp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    pub state: String,
    pub can_invite: bool,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodDiscoveryList(pub Vec<PodDiscoveryRowDto>);

// ── pod.pending ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPendingOfferDto {
    pub offer_id: String,
    pub direction: String,
    pub peer_pubkey_fp: String,
    pub peer_hostname: String,
    pub peer_addr: String,
    pub peer_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inviter_peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    pub expires_at: i64,
    pub ttl_secs: i64,
    pub created_at: i64,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodPendingList(pub Vec<PodPendingOfferDto>);

// ── pod.offer ────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodOfferArgs {
    /// Joiner's bootstrap address (host or host:port). Joiner must already
    /// be in `pod_discovery` (mDNS-seen) so we know its pinned pubkey fp.
    pub addr: String,
    /// Optional override for the joiner's bootstrap port. Defaults to
    /// `APP_PLUGIN_PORT` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodOfferOutput {
    /// Pairing code minted for this offer; show to the operator so they can
    /// run `pod.accept` on the joiner side.
    pub code: String,
    pub joiner_hostname: String,
    pub joiner_addr: String,
    pub joiner_port: u16,
    pub joiner_pubkey_fp: String,
    pub offer_id: String,
    pub expires_at: i64,
}

// ── pod.join ─────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinArgs {
    /// Inviter's address (host or host:port).
    pub inviter_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinOutput {
    pub code: String,
    pub inviter_addr: String,
    pub inviter_port: u16,
}

// ── pod.leave ────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveArgs {
    /// Peer to notify + remove. The full `pod leave` wipe path stays on the
    /// CLI (it touches secrets + PKI material and takes flags this tool
    /// purposely doesn't expose).
    pub peer_id: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveOutput {
    pub peer_id: String,
    pub notify_result: String,
    pub rows_removed: u32,
}

// ── pod.cert-status ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CertInfo {
    pub cn: String,
    pub fingerprint: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub days_remaining: i64,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodCertStatusOutput {
    pub founder: bool,
    pub member: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesh_ca: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leaf_server: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leaf_client: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_previous: Option<CertInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<CertInfo>,
}

// ── Native support: From impls, PodService trait, svc() helper ──────────────

#[cfg(feature = "native")]
mod native_support {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_db as db;
    use orca_utils::tool::ToolCtx;
    use std::sync::Arc;

    impl From<db::host_addressing::PodPeerAddress> for PodPeerAddressDto {
        fn from(a: db::host_addressing::PodPeerAddress) -> Self {
            Self {
                kind: a.kind,
                value: a.value,
                source: a.source,
                last_seen_at: a.last_seen_at,
            }
        }
    }

    impl From<db::pod::PeerSummary> for PodPeerDto {
        fn from(p: db::pod::PeerSummary) -> Self {
            Self {
                peer_id: p.peer_id,
                hostname: p.hostname,
                addr: p.addr,
                port: p.port,
                last_seen_at: p.last_seen_at,
                local_secure: p.local_secure,
                peer_secure: p.peer_secure,
                status: p.status,
                addresses: p.addresses.into_iter().map(Into::into).collect(),
            }
        }
    }

    /// Service hook the server registers at startup. tools-def stays
    /// wasm-safe — every mTLS dial, PKI read, and bootstrap signing op lives
    /// behind this trait so the daemon owns all the network/process state.
    #[async_trait]
    pub trait PodService: Send + Sync {
        async fn accept(&self, code: &str) -> Result<PodAcceptOutput>;
        async fn trust(&self, peer_id: &str, on: bool) -> Result<PodTrustOutput>;
        async fn ping(&self, peer_id: &str) -> PodPingOutput;
        fn discover(&self) -> Result<Vec<PodDiscoveryRowDto>>;
        fn pending(&self) -> Result<Vec<PodPendingOfferDto>>;
        async fn offer(&self, addr: &str, port: Option<u16>) -> Result<PodOfferOutput>;
        async fn join(&self, inviter_addr: &str, port: Option<u16>) -> Result<PodJoinOutput>;
        async fn leave_peer(&self, peer_id: &str) -> Result<PodLeaveOutput>;
        fn cert_status(&self) -> Result<PodCertStatusOutput>;
    }

    pub(super) fn svc(ctx: &ToolCtx) -> Result<Arc<dyn PodService>> {
        ctx.service::<Arc<dyn PodService>>()
    }
}

#[cfg(feature = "native")]
pub use native_support::PodService;

// ── Tools ───────────────────────────────────────────────────────────────────

/// List paired pod peers (mesh members).
#[orca_tool(domain = "pod", verb = "list")]
async fn pod_list(
    _args: EmptyArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodPeerList> {
    let conn = orca_db::open_default()?;
    Ok(PodPeerList(
        orca_db::pod::list_peers(&conn)?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

/// Accept a pending pod-membership offer by pairing code.
#[orca_tool(domain = "pod", verb = "accept")]
async fn pod_accept(
    args: PodAcceptArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodAcceptOutput> {
    native_support::svc(ctx)?.accept(&args.code).await
}

/// Toggle local trust for a paired peer; replicates CA key on mutual-secure.
#[orca_tool(domain = "pod", verb = "trust")]
async fn pod_trust(
    args: PodTrustArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodTrustOutput> {
    native_support::svc(ctx)?
        .trust(&args.peer_id, args.on)
        .await
}

/// mTLS ping a paired peer; returns latency + their self-reported identity.
#[orca_tool(domain = "pod", verb = "ping")]
async fn pod_ping(
    args: PodPingArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodPingOutput> {
    Ok(native_support::svc(ctx)?.ping(&args.peer_id).await)
}

/// List orcas seen on the network via mDNS (paired + unclaimed).
#[orca_tool(domain = "pod", verb = "discover")]
async fn pod_discover(
    _args: EmptyArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodDiscoveryList> {
    Ok(PodDiscoveryList(native_support::svc(ctx)?.discover()?))
}

/// List pending inbound pod-membership offers.
#[orca_tool(domain = "pod", verb = "pending")]
async fn pod_pending(
    _args: EmptyArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodPendingList> {
    Ok(PodPendingList(native_support::svc(ctx)?.pending()?))
}

/// Push a pod-membership offer to a discovered joiner.
#[orca_tool(domain = "pod", verb = "offer")]
async fn pod_offer(
    args: PodOfferArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodOfferOutput> {
    native_support::svc(ctx)?.offer(&args.addr, args.port).await
}

/// Joiner-initiated pair: request an offer from an out-of-mDNS inviter.
#[orca_tool(domain = "pod", verb = "join")]
async fn pod_join(
    args: PodJoinArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodJoinOutput> {
    native_support::svc(ctx)?
        .join(&args.inviter_addr, args.port)
        .await
}

/// Best-effort notify a peer we're leaving, then drop pod_peers + pod_trust rows for it.
#[orca_tool(domain = "pod", verb = "leave")]
async fn pod_leave(
    args: PodLeaveArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodLeaveOutput> {
    native_support::svc(ctx)?.leave_peer(&args.peer_id).await
}

/// Days-remaining + rotation state for every mesh cert on this host.
#[orca_tool(domain = "pod", verb = "cert-status")]
async fn pod_cert_status(
    _args: EmptyArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodCertStatusOutput> {
    native_support::svc(ctx)?.cert_status()
}
