//! Pod / mesh tools surfaced to the four-surface registry.
//!
//! `pod.list` mirrors the CLI's `orca pod list` so the web overview can
//! render paired peers without a bespoke REST endpoint. The remaining ops
//! delegate to `PodService` (registered by the server) because they need
//! mTLS dials, PKI material, and bootstrap signing — all server-side state
//! that this crate must not touch directly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Args / Output types (shared by every surface) ────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPeerAddressDto {
    pub kind: String,
    pub value: String,
    pub source: String,
    pub last_seen_at: i64,
}

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
    /// True for the synthetic local-host row prepended to `pod.list`. Remote
    /// peers are always false. Lets UIs flag "this is me" without string
    /// matching the hostname.
    #[serde(default)]
    pub local: bool,
    /// `pod/ping` succeeded inside the fanout budget. `None` when probing was
    /// skipped (e.g. departed peers); `Some(false)` when the dial errored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
    /// Round-trip latency of the `pod/ping` probe, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// Error string from the probe path (ping / runtime-spec / update-check).
    /// First failure wins so the UI has one line to surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
    /// Peer-reported `system.runtime-spec.version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Peer-reported build target triple.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Peer-reported "embedded" / "disabled" UI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontend: Option<String>,
    /// Peer-reported daemon mode: "daemon" | "parked" | "dev".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Peer-reported release channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Peer-reported version pin if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
    /// Latest release tag visible to the peer on its channel. Pulled from
    /// `system.update-check`; `None` when the probe failed or timed out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_latest: Option<String>,
    /// True when an update is available for the peer (and not blocked by
    /// `pinned_to`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_available: Option<bool>,
    /// Cross-platform OS / hardware / process / network snapshot reported
    /// by the peer's `system.runtime-spec`. `None` when the probe failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<crate::orca_lifecycle::SystemInfoReport>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodPeerList(pub Vec<PodPeerDto>);

// ── pod.dev.sync ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDevSyncPeerResult {
    pub peer_id: String,
    pub hostname: String,
    /// "synced" | "skipped" | "error"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDevSyncOutput {
    pub results: Vec<PodDevSyncPeerResult>,
}

// ── pod.dev.enable / pod.dev.disable ────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Default, Serialize, Deserialize, JsonSchema)]
pub struct PodDevFanoutArgs {
    /// Subset of peer hostnames (or addrs) to target. Empty = every paired
    /// peer plus this host.
    #[cfg_attr(feature = "cli", clap(long))]
    #[serde(default)]
    pub peers: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDevEnablePeerResult {
    pub peer_id: String,
    pub hostname: String,
    /// "enabled" | "error"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDevEnableOutput {
    pub results: Vec<PodDevEnablePeerResult>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDevDisablePeerResult {
    pub peer_id: String,
    pub hostname: String,
    /// "disabled" | "error"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodDevDisableOutput {
    pub results: Vec<PodDevDisablePeerResult>,
}

// ── pod.accept ───────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodAcceptArgs {
    /// 6-char pairing code shown on the inviter's screen.
    pub code: String,
}

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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodTrustArgs {
    pub peer_id: String,
    pub on: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodTrustOutput {
    pub peer_id: String,
    pub local_secure: bool,
    pub peer_secure: bool,
    pub mutual: bool,
    pub notify_result: String,
}

// ── pod.ping ─────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPingArgs {
    /// Paired peer ID (`peer.<machine_id_short>`) — looked up in `pod_peers`
    /// for the dial target.
    pub peer_id: String,
}

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

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodDiscoveryList(pub Vec<PodDiscoveryRowDto>);

// ── pod.pending ──────────────────────────────────────────────────────────────

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

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodPendingList(pub Vec<PodPendingOfferDto>);

// ── pod.offer ────────────────────────────────────────────────────────────────

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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinArgs {
    /// Inviter's address (host or host:port).
    pub inviter_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinOutput {
    pub code: String,
    pub inviter_addr: String,
    pub inviter_port: u16,
}

// ── pod.leave ────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveArgs {
    /// Peer to notify + remove. The full `pod leave` wipe path stays on the
    /// CLI (it touches secrets + PKI material and takes flags this tool
    /// purposely doesn't expose).
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveOutput {
    pub peer_id: String,
    pub notify_result: String,
    pub rows_removed: u32,
}

// ── pod.cert-status ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CertInfo {
    pub cn: String,
    pub fingerprint: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub days_remaining: i64,
}

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
pub mod native_support {
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
                local: false,
                reachable: None,
                latency_ms: None,
                probe_error: None,
                version: None,
                target: None,
                frontend: None,
                mode: None,
                channel: None,
                pinned_to: None,
                update_latest: None,
                update_available: None,
                system: None,
            }
        }
    }

    /// Service hook the server registers at startup. tools-def stays
    /// — every mTLS dial, PKI read, and bootstrap signing op lives
    /// behind this trait so the daemon owns all the network/process state.
    #[async_trait]
    pub trait PodService: Send + Sync {
        /// Enriched peer list used by `pod.list`. Adds a synthetic local row
        /// (built from this host's runtime spec) and fans out `pod/ping`,
        /// `system.runtime-spec`, and `system.update-check` over the mesh to
        /// fill the per-peer optional fields. Failed probes leave fields
        /// `None` with `probe_error` populated; the call never errors solely
        /// because a peer is unreachable.
        async fn list_enriched(&self) -> Result<Vec<PodPeerDto>>;
        async fn accept(&self, code: &str) -> Result<PodAcceptOutput>;
        async fn trust(&self, peer_id: &str, on: bool) -> Result<PodTrustOutput>;
        async fn ping(&self, peer_id: &str) -> PodPingOutput;
        fn discover(&self) -> Result<Vec<PodDiscoveryRowDto>>;
        fn pending(&self) -> Result<Vec<PodPendingOfferDto>>;
        async fn offer(&self, addr: &str, port: Option<u16>) -> Result<PodOfferOutput>;
        async fn join(&self, inviter_addr: &str, port: Option<u16>) -> Result<PodJoinOutput>;
        async fn leave_peer(&self, peer_id: &str) -> Result<PodLeaveOutput>;
        fn cert_status(&self) -> Result<PodCertStatusOutput>;
        async fn dev_sync(&self) -> Result<PodDevSyncOutput>;
        async fn dev_enable_fanout(&self, peers: &[String]) -> Result<PodDevEnableOutput>;
        async fn dev_disable_fanout(&self, peers: &[String]) -> Result<PodDevDisableOutput>;
        // Wire-level JSON-RPC dispatch — Value here is the on-wire payload,
        // narrowed back to the tool's typed `OrcaToolDef::Output` inside
        // [`crate::cli::exec_remote`] before reaching any user code.
        #[allow(clippy::disallowed_types)]
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: serde_json::Value,
        ) -> Result<PodExecDispatch>;
    }

    /// Internal-only envelope for [`PodService::exec`]. JSON `Value` here is
    /// the JSON-RPC wire payload — type-erased only because the peer-side
    /// registry dispatches by name. Callers go through
    /// [`crate::cli::exec_remote`], which deserializes into the typed
    /// `OrcaToolDef::Output` immediately on receipt, so no opaque value ever
    /// reaches a user-facing type.
    #[allow(clippy::disallowed_types)]
    pub struct PodExecDispatch {
        pub peer: String,
        pub tool: String,
        pub result: serde_json::Value,
    }

    pub fn svc(ctx: &ToolCtx) -> Result<Arc<dyn PodService>> {
        ctx.service::<Arc<dyn PodService>>()
    }
}

#[cfg(feature = "native")]
pub use native_support::{PodExecDispatch, PodService};

#[cfg(feature = "native")]
pub trait ProvidePod {
    fn pod(&self) -> std::sync::Arc<dyn PodService>;
}

#[cfg(feature = "native")]
pub fn register_pod(ctx: &mut orca_utils::tool::ToolCtx, p: &impl ProvidePod) {
    ctx.register_service(p.pod());
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// List paired pod peers (mesh members).
#[orca_tool(domain = "pod", verb = "list", remote_ok = true)]
async fn pod_list(
    _args: EmptyArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodPeerList> {
    Ok(PodPeerList(
        native_support::svc(ctx)?.list_enriched().await?,
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

/// git pull on every active peer running in dev mode; cargo watch auto-restarts.
/// Peers not in dev mode are skipped (not an error).
#[orca_tool(domain = "pod", verb = "dev_sync", role = "admin")]
async fn pod_dev_sync(
    _args: EmptyArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodDevSyncOutput> {
    native_support::svc(ctx)?.dev_sync().await
}

/// Flip dev mode ON across the mesh. Empty `peers` = local + every paired
/// peer. Each peer clones the repo if needed and spawns cargo-watch.
#[orca_tool(domain = "pod", verb = "dev_enable", role = "admin")]
async fn pod_dev_enable(
    args: PodDevFanoutArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodDevEnableOutput> {
    native_support::svc(ctx)?
        .dev_enable_fanout(&args.peers)
        .await
}

/// Flip dev mode OFF across the mesh. Empty `peers` = local + every paired
/// peer. Each peer stops cargo-watch and the production daemon reclaims.
#[orca_tool(domain = "pod", verb = "dev_disable", role = "admin")]
async fn pod_dev_disable(
    args: PodDevFanoutArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PodDevDisableOutput> {
    native_support::svc(ctx)?
        .dev_disable_fanout(&args.peers)
        .await
}
