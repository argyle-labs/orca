//! Pod / mesh tools surfaced to the four-surface registry.
//!
//! `pod.list` mirrors the CLI's `orca pod list` so the web overview can
//! render paired peers without a bespoke REST endpoint. The remaining ops
//! delegate to `PodService` (registered by the server) because they need
//! mTLS dials, PKI material, and bootstrap signing — all server-side state
//! that this crate must not touch directly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use orca_macro::orca_tool;

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
    pub system: Option<crate::lifecycle::SystemInfoReport>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodPeerListOutput(pub Vec<PodPeerDto>);

// ── system.peer.create — unified pairing entry point ─────────────────────────
//
// `action` selects the pairing role:
//   "invite"  — inviter pushes offer to a discovered joiner  (needs `addr`)
//   "join"    — joiner pulls offer from an out-of-mDNS host  (needs `addr`)
//   "accept"  — joiner accepts a pending inbound offer        (needs `code`)

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PeerCreateArgs {
    /// "invite" | "join" | "accept"
    pub action: String,
    /// Target address (host or host:port). Required for "invite" and "join".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub addr: Option<String>,
    /// Override port. Defaults to `APP_PLUGIN_PORT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub port: Option<u16>,
    /// 6-char pairing code. Required for "accept".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub code: Option<String>,
}

/// Unified output for all three pairing roles. Only the fields relevant to
/// the chosen `action` are populated; the rest are omitted.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PeerCreateOutput {
    pub action: String,
    // invite fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joiner_hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joiner_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joiner_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joiner_pubkey_fp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    // accept fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inviter_peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inviter_hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inviter_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inviter_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_secure: Option<bool>,
}

// kept for internal use by accept path
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodAcceptOutput {
    pub pod_id: String,
    pub inviter_peer_id: String,
    pub inviter_hostname: String,
    pub inviter_addr: String,
    pub inviter_port: u16,
    pub self_secure: bool,
}

// ── pod.trust ────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodTrustArgs {
    pub peer_id: String,
    // Bare `bool` derives as a flag (`--on`) under clap, which leaves no way
    // to express the positional `[ON]` shown in --help. Force value parsing
    // so `orca system peer update <peer> true|false` works.
    #[cfg_attr(feature = "cli", clap(action = clap::ArgAction::Set))]
    pub on: bool,
    /// When `true`, execute the trust update on the remote peer so THEY trust
    /// US rather than updating our local trust of them. Requires the peer to
    /// be reachable via mTLS and the caller to hold admin role.
    #[serde(default)]
    #[cfg_attr(feature = "cli", clap(long))]
    pub push: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodTrustOutput {
    pub peer_id: String,
    pub local_secure: bool,
    pub peer_secure: bool,
    /// True when both sides trust each other. Secure peers can sync
    /// credentials; non-mutual peers only retain their own credentials.
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
pub struct PodDiscoveryListOutput(pub Vec<PodDiscoveryRowDto>);

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
pub struct PodPendingListOutput(pub Vec<PodPendingOfferDto>);

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
    /// Tier-2 secrets-storage permission. When `true`, this host is authorized
    /// to hold encrypted secrets replicated from other pod members. Independent
    /// of cert trust — a fully paired host can still refuse to be a secrets
    /// sink. UI surfaces this as a Secrets-storage toggle distinct from Trust.
    #[serde(default)]
    pub self_secure: bool,
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

// ── system.pod.update — singleton pod-settings update ───────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodUpdateArgs {
    /// Toggle Tier-2 secrets-storage permission (`self_secure`). `None` leaves
    /// the current value unchanged so the tool can grow new fields without
    /// every caller having to opt out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub self_secure: Option<bool>,
    /// When set, proxy the call to the named remote peer via the pod mesh
    /// instead of running on the local host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long, hide = true))]
    pub peer_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodUpdateOutput {
    pub self_secure: bool,
}

// ── Native support: From impls, PodService trait, svc() helper ──────────────

#[cfg(feature = "native")]
pub mod native_support {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_contract::ToolCtx;
    use orca_db as db;
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

    /// Service hook the server registers at startup. `fleet` stays
    /// transport-neutral — every mTLS dial, PKI read, and bootstrap signing
    /// op lives behind this trait so the daemon owns all the network/process
    /// state.
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
        /// Update our local trust of a peer. Sets `local_secure`.
        async fn trust(&self, peer_id: &str, on: bool) -> Result<PodTrustOutput>;
        /// Push a trust update to a remote peer over mTLS, making THEM trust
        /// US. Executes `pod.peer.update` on the remote host with our own
        /// peer_id. Returns the merged trust state after the push.
        async fn push_trust(&self, peer_id: &str, on: bool) -> Result<PodTrustOutput>;
        async fn ping(&self, peer_id: &str) -> PodPingOutput;
        fn discover(&self) -> Result<Vec<PodDiscoveryRowDto>>;
        fn pending(&self) -> Result<Vec<PodPendingOfferDto>>;
        async fn offer(&self, addr: &str, port: Option<u16>) -> Result<PodOfferOutput>;
        async fn join(&self, inviter_addr: &str, port: Option<u16>) -> Result<PodJoinOutput>;
        async fn leave_peer(&self, peer_id: &str) -> Result<PodLeaveOutput>;
        fn cert_status(&self) -> Result<PodCertStatusOutput>;
        /// Read `self_secure`. Used by `system.pod.detail` enrichment so the
        /// UI shows the current Tier-2 secrets-storage state alongside cert
        /// info.
        fn get_self_secure(&self) -> Result<bool>;
        /// Update `self_secure`. Idempotent: passing the current value is a
        /// no-op. Returns the resulting value.
        async fn set_self_secure(&self, on: bool) -> Result<bool>;
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

    /// Adapter that lets the generic `orca_contract::RemoteExec` trait
    /// dispatch through `PodService::exec`. Registered alongside the
    /// `PodService` so `cli::exec_remote::<T>(...)` (which lives in
    /// `orca-dispatch` and knows nothing about pod) finds a transport.
    #[cfg(feature = "cli")]
    pub struct PodRemoteExec(pub Arc<dyn PodService>);

    #[cfg(feature = "cli")]
    #[async_trait]
    impl orca_contract::RemoteExec for PodRemoteExec {
        #[allow(clippy::disallowed_types)]
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: serde_json::Value,
        ) -> Result<serde_json::Value> {
            let dispatch = self.0.exec(peer, tool, args).await?;
            Ok(dispatch.result)
        }
    }
}

#[cfg(feature = "native")]
pub use native_support::{PodExecDispatch, PodService};

#[cfg(feature = "native")]
pub trait ProvidePod {
    fn pod(&self) -> std::sync::Arc<dyn PodService>;
}

#[cfg(feature = "native")]
pub fn register_pod(ctx: &mut orca_contract::ToolCtx, p: &impl ProvidePod) {
    let pod = p.pod();
    ctx.register_service(pod.clone());
    // Register the RemoteExec adapter so `cli::exec_remote::<T>(...)` (in
    // orca-dispatch) can dispatch through PodService::exec without
    // orca-dispatch depending on the `fleet` domain crate.
    #[cfg(feature = "cli")]
    {
        let remote: std::sync::Arc<dyn orca_contract::RemoteExec> =
            std::sync::Arc::new(native_support::PodRemoteExec(pod));
        ctx.register_service(remote);
    }
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// List paired pod peers (mesh members).
#[orca_tool(domain = "system.peer", verb = "list", remote_ok = true)]
async fn pod_peer_list(
    _args: EmptyArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodPeerListOutput> {
    Ok(PodPeerListOutput(
        native_support::svc(ctx)?.list_enriched().await?,
    ))
}

/// Initiate or complete a peer pairing.
///
/// `action`:
/// - `"invite"` — inviter pushes an offer to a discovered joiner. Requires
///   `addr` (joiner's host or host:port from mDNS discovery). Returns a
///   pairing code to show the operator; the joiner auto-accepts if its daemon
///   received the code in-band.
/// - `"join"` — joiner requests an offer from an inviter not yet in mDNS.
///   Requires `addr` (inviter's host or host:port). Returns the code the
///   inviter will display.
/// - `"accept"` — joiner accepts a pending inbound offer by its 6-char code.
///   Requires `code`. Returns the inviter identity after join.
#[orca_tool(domain = "system.peer", verb = "create")]
async fn peer_create(
    args: PeerCreateArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PeerCreateOutput> {
    let svc = native_support::svc(ctx)?;
    match args.action.as_str() {
        "invite" => {
            let addr = args
                .addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("invite requires addr"))?;
            let out = svc.offer(addr, args.port).await?;
            Ok(PeerCreateOutput {
                action: "invite".into(),
                pairing_code: Some(out.code),
                joiner_hostname: Some(out.joiner_hostname),
                joiner_addr: Some(out.joiner_addr),
                joiner_port: Some(out.joiner_port),
                joiner_pubkey_fp: Some(out.joiner_pubkey_fp),
                offer_id: Some(out.offer_id),
                expires_at: Some(out.expires_at),
                pod_id: None,
                inviter_peer_id: None,
                inviter_hostname: None,
                inviter_addr: None,
                inviter_port: None,
                self_secure: None,
            })
        }
        "join" => {
            let addr = args
                .addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("join requires addr"))?;
            let out = svc.join(addr, args.port).await?;
            Ok(PeerCreateOutput {
                action: "join".into(),
                pairing_code: Some(out.code),
                joiner_hostname: None,
                joiner_addr: None,
                joiner_port: None,
                joiner_pubkey_fp: None,
                offer_id: None,
                expires_at: None,
                pod_id: None,
                inviter_peer_id: None,
                inviter_hostname: None,
                inviter_addr: Some(out.inviter_addr),
                inviter_port: Some(out.inviter_port),
                self_secure: None,
            })
        }
        "accept" => {
            let code = args
                .code
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("accept requires code"))?;
            let out = svc.accept(code).await?;
            Ok(PeerCreateOutput {
                action: "accept".into(),
                pairing_code: None,
                joiner_hostname: None,
                joiner_addr: None,
                joiner_port: None,
                joiner_pubkey_fp: None,
                offer_id: None,
                expires_at: None,
                pod_id: Some(out.pod_id),
                inviter_peer_id: Some(out.inviter_peer_id),
                inviter_hostname: Some(out.inviter_hostname),
                inviter_addr: Some(out.inviter_addr),
                inviter_port: Some(out.inviter_port),
                self_secure: Some(out.self_secure),
            })
        }
        other => anyhow::bail!("unknown action '{other}' (expected invite|join|accept)"),
    }
}

/// Update trust for a paired peer.
/// Without `push`: sets OUR local trust of the peer (`local_secure`).
/// With `push: true`: executes the update on the remote peer over mTLS so
/// THEY trust US (`peer_secure` from our perspective).
#[orca_tool(domain = "system.peer", verb = "update")]
async fn pod_peer_update(
    args: PodTrustArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodTrustOutput> {
    let svc = native_support::svc(ctx)?;
    if args.push {
        return svc.push_trust(&args.peer_id, args.on).await;
    }
    svc.trust(&args.peer_id, args.on).await
}

/// mTLS ping a paired peer; returns latency + their self-reported identity.
#[orca_tool(domain = "system.peer", verb = "detail")]
async fn pod_peer_detail(
    args: PodPingArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodPingOutput> {
    Ok(native_support::svc(ctx)?.ping(&args.peer_id).await)
}

/// List orcas seen on the network via mDNS (paired + unclaimed).
#[orca_tool(domain = "system.peer.discovery", verb = "list")]
async fn pod_discovery_list(
    _args: EmptyArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodDiscoveryListOutput> {
    Ok(PodDiscoveryListOutput(
        native_support::svc(ctx)?.discover()?,
    ))
}

/// List pending inbound pod-membership offers.
#[orca_tool(domain = "system.peer.handshake", verb = "list")]
async fn pod_handshake_list(
    _args: EmptyArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodPendingListOutput> {
    Ok(PodPendingListOutput(native_support::svc(ctx)?.pending()?))
}

/// Best-effort notify a peer we're leaving, then drop pod_peers + pod_trust rows for it.
#[orca_tool(domain = "system.peer", verb = "delete")]
async fn pod_peer_delete(
    args: PodLeaveArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodLeaveOutput> {
    native_support::svc(ctx)?.leave_peer(&args.peer_id).await
}

/// Days-remaining + rotation state for every mesh cert on this host, plus
/// the current `self_secure` (Tier-2 secrets-storage) setting.
#[orca_tool(domain = "system.pod", verb = "detail")]
async fn pod_detail(
    _args: EmptyArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodCertStatusOutput> {
    let svc = native_support::svc(ctx)?;
    let mut out = svc.cert_status()?;
    out.self_secure = svc.get_self_secure().unwrap_or(false);
    Ok(out)
}

/// Update pod-level settings on this host or — when `peer_id` is set —
/// on the named remote peer over the pod mesh. Currently exposes
/// `self_secure` (Tier-2 secrets-storage permission). Admin-only because
/// flipping it can authorize secrets replication into this host.
#[orca_tool(
    domain = "system.pod",
    verb = "update",
    role = "admin",
    remote_ok = true
)]
async fn pod_update(
    args: PodUpdateArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodUpdateOutput> {
    if let Some(ref peer_id) = args.peer_id {
        let dispatch = native_support::svc(ctx)?
            .exec(
                peer_id,
                "system.pod.update",
                serde_json::json!({ "self_secure": args.self_secure }),
            )
            .await?;
        return Ok(serde_json::from_value(dispatch.result)?);
    }
    let svc = native_support::svc(ctx)?;
    let self_secure = match args.self_secure {
        Some(v) => svc.set_self_secure(v).await?,
        None => svc.get_self_secure()?,
    };
    Ok(PodUpdateOutput { self_secure })
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::native_support::PodExecDispatch;
    use super::*;
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct StubPod {
        last_accept_code: Mutex<Option<String>>,
        last_trust: Mutex<Option<(String, bool)>>,
        last_ping_peer: Mutex<Option<String>>,
        last_offer: Mutex<Option<(String, Option<u16>)>>,
        last_join: Mutex<Option<(String, Option<u16>)>>,
        last_leave_peer: Mutex<Option<String>>,
        self_secure: Mutex<bool>,
        // Mirrors PodService::exec — peer-mesh wire payload is type-erased
        // at the dispatch boundary. Same justification as the trait method.
        #[allow(clippy::disallowed_types)]
        last_exec: Mutex<Option<(String, String, serde_json::Value)>>,
    }

    #[async_trait]
    impl PodService for StubPod {
        async fn list_enriched(&self) -> Result<Vec<PodPeerDto>> {
            Ok(vec![PodPeerDto {
                peer_id: "peer.abc".into(),
                hostname: "willow".into(),
                addr: "10.0.0.1".into(),
                port: 12002,
                last_seen_at: 0,
                local_secure: true,
                peer_secure: true,
                status: "active".into(),
                addresses: vec![],
                local: false,
                reachable: Some(true),
                latency_ms: Some(7),
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
            }])
        }
        async fn accept(&self, code: &str) -> Result<PodAcceptOutput> {
            *self.last_accept_code.lock().unwrap() = Some(code.into());
            Ok(PodAcceptOutput {
                pod_id: "pod-1".into(),
                inviter_peer_id: "peer.inv".into(),
                inviter_hostname: "mint".into(),
                inviter_addr: "10.0.0.2".into(),
                inviter_port: 12002,
                self_secure: false,
            })
        }
        async fn trust(&self, peer_id: &str, on: bool) -> Result<PodTrustOutput> {
            *self.last_trust.lock().unwrap() = Some((peer_id.into(), on));
            Ok(PodTrustOutput {
                peer_id: peer_id.into(),
                local_secure: on,
                peer_secure: true,
                mutual: on,
                notify_result: "ok".into(),
            })
        }
        async fn push_trust(&self, peer_id: &str, on: bool) -> Result<PodTrustOutput> {
            Ok(PodTrustOutput {
                peer_id: peer_id.into(),
                local_secure: false,
                peer_secure: on,
                mutual: on,
                notify_result: "pushed".into(),
            })
        }
        async fn ping(&self, peer_id: &str) -> PodPingOutput {
            *self.last_ping_peer.lock().unwrap() = Some(peer_id.into());
            PodPingOutput {
                ok: true,
                latency_ms: 3,
                error: None,
                peer_id: Some(peer_id.into()),
                hostname: Some("loki".into()),
                version: Some("0.0.0".into()),
            }
        }
        fn discover(&self) -> Result<Vec<PodDiscoveryRowDto>> {
            Ok(vec![PodDiscoveryRowDto {
                pubkey_fp: "fp".into(),
                peer_id: None,
                hostname: "freyr".into(),
                addr: "10.0.0.3".into(),
                port: 12002,
                state: "seen".into(),
                can_invite: true,
                first_seen_at: 0,
                last_seen_at: 0,
            }])
        }
        fn pending(&self) -> Result<Vec<PodPendingOfferDto>> {
            Ok(vec![])
        }
        async fn offer(&self, addr: &str, port: Option<u16>) -> Result<PodOfferOutput> {
            *self.last_offer.lock().unwrap() = Some((addr.into(), port));
            Ok(PodOfferOutput {
                code: "ABC123".into(),
                joiner_hostname: "thor".into(),
                joiner_addr: addr.into(),
                joiner_port: port.unwrap_or(12002),
                joiner_pubkey_fp: "fp".into(),
                offer_id: "oid".into(),
                expires_at: 0,
            })
        }
        async fn join(&self, inviter_addr: &str, port: Option<u16>) -> Result<PodJoinOutput> {
            *self.last_join.lock().unwrap() = Some((inviter_addr.into(), port));
            Ok(PodJoinOutput {
                code: "XYZ".into(),
                inviter_addr: inviter_addr.into(),
                inviter_port: port.unwrap_or(12002),
            })
        }
        async fn leave_peer(&self, peer_id: &str) -> Result<PodLeaveOutput> {
            *self.last_leave_peer.lock().unwrap() = Some(peer_id.into());
            Ok(PodLeaveOutput {
                peer_id: peer_id.into(),
                notify_result: "ok".into(),
                rows_removed: 2,
            })
        }
        fn cert_status(&self) -> Result<PodCertStatusOutput> {
            Ok(PodCertStatusOutput {
                founder: true,
                member: true,
                self_secure: false,
                mesh_ca: None,
                leaf_server: None,
                leaf_client: None,
                ca_previous: None,
                bootstrap: None,
            })
        }
        fn get_self_secure(&self) -> Result<bool> {
            Ok(*self.self_secure.lock().unwrap())
        }
        async fn set_self_secure(&self, on: bool) -> Result<bool> {
            *self.self_secure.lock().unwrap() = on;
            Ok(on)
        }
        #[allow(clippy::disallowed_types)] // mirrors trait — peer-mesh wire payload
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: serde_json::Value,
        ) -> Result<PodExecDispatch> {
            *self.last_exec.lock().unwrap() = Some((peer.into(), tool.into(), args.clone()));
            Ok(PodExecDispatch {
                peer: peer.into(),
                tool: tool.into(),
                result: serde_json::json!({"ok": true}),
            })
        }
    }

    fn ctx_with_stub() -> (orca_contract::ToolCtx, Arc<StubPod>) {
        let stub = Arc::new(StubPod::default());
        let svc: Arc<dyn PodService> = stub.clone();
        let mut ctx = empty_ctx();
        ctx.register_service(svc);
        (ctx, stub)
    }

    #[test]
    fn pod_peer_address_from_db_row() {
        let row = orca_db::host_addressing::PodPeerAddress {
            peer_id: "peer.x".into(),
            kind: "lan_v4".into(),
            value: "10.0.0.5".into(),
            source: "mdns".into(),
            last_seen_at: 42,
        };
        let dto: PodPeerAddressDto = row.into();
        assert_eq!(dto.kind, "lan_v4");
        assert_eq!(dto.value, "10.0.0.5");
        assert_eq!(dto.source, "mdns");
        assert_eq!(dto.last_seen_at, 42);
    }

    #[test]
    fn pod_peer_from_db_summary_defaults_optional_fields_to_none() {
        let row = orca_db::pod::PeerSummary {
            peer_id: "peer.x".into(),
            hostname: "h".into(),
            addr: "1.2.3.4".into(),
            port: 12002,
            last_seen_at: 1,
            local_secure: true,
            peer_secure: false,
            status: "active".into(),
            addresses: vec![],
        };
        let dto: PodPeerDto = row.into();
        assert_eq!(dto.peer_id, "peer.x");
        assert!(!dto.local);
        assert!(dto.reachable.is_none());
        assert!(dto.version.is_none());
        assert!(dto.system.is_none());
    }

    #[tokio::test]
    async fn pod_list_forwards_to_service() {
        let (ctx, _) = ctx_with_stub();
        let out = pod_peer_list(EmptyArgs {}, &ctx).await.unwrap();
        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].peer_id, "peer.abc");
    }

    #[tokio::test]
    async fn pod_accept_forwards_code() {
        let (ctx, stub) = ctx_with_stub();
        let out = peer_create(
            PeerCreateArgs {
                action: "accept".into(),
                addr: None,
                port: None,
                code: Some("code1".into()),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.action, "accept");
        assert_eq!(out.pod_id.as_deref(), Some("pod-1"));
        assert_eq!(
            stub.last_accept_code.lock().unwrap().as_deref(),
            Some("code1")
        );
    }

    #[tokio::test]
    async fn pod_trust_forwards_peer_and_flag() {
        let (ctx, stub) = ctx_with_stub();
        let out = pod_peer_update(
            PodTrustArgs {
                peer_id: "peer.t".into(),
                on: true,
                push: false,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.mutual);
        let g = stub.last_trust.lock().unwrap();
        assert_eq!(g.as_ref().unwrap(), &("peer.t".to_string(), true));
    }

    #[tokio::test]
    async fn pod_trust_push_routes_to_push_trust() {
        let (ctx, _) = ctx_with_stub();
        let out = pod_peer_update(
            PodTrustArgs {
                peer_id: "peer.t".into(),
                on: true,
                push: true,
            },
            &ctx,
        )
        .await
        .unwrap();
        // StubPod::push_trust returns peer_secure=on (true), local_secure=false.
        assert!(out.peer_secure);
        assert!(!out.local_secure);
        assert_eq!(out.notify_result, "pushed");
    }

    #[tokio::test]
    async fn pod_ping_forwards_peer() {
        let (ctx, stub) = ctx_with_stub();
        let out = pod_peer_detail(
            PodPingArgs {
                peer_id: "peer.p".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.ok);
        assert_eq!(
            stub.last_ping_peer.lock().unwrap().as_deref(),
            Some("peer.p")
        );
    }

    #[tokio::test]
    async fn pod_discover_wraps_service_rows() {
        let (ctx, _) = ctx_with_stub();
        let out = pod_discovery_list(EmptyArgs {}, &ctx).await.unwrap();
        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].hostname, "freyr");
    }

    #[tokio::test]
    async fn pod_pending_wraps_service_rows() {
        let (ctx, _) = ctx_with_stub();
        let out = pod_handshake_list(EmptyArgs {}, &ctx).await.unwrap();
        assert!(out.0.is_empty());
    }

    #[tokio::test]
    async fn pod_offer_forwards_addr_and_port() {
        let (ctx, stub) = ctx_with_stub();
        let out = peer_create(
            PeerCreateArgs {
                action: "invite".into(),
                addr: Some("1.2.3.4".into()),
                port: Some(9999),
                code: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.action, "invite");
        assert_eq!(out.joiner_port, Some(9999));
        let g = stub.last_offer.lock().unwrap();
        assert_eq!(g.as_ref().unwrap(), &("1.2.3.4".to_string(), Some(9999)));
    }

    #[tokio::test]
    async fn pod_join_forwards_inviter_and_port() {
        let (ctx, stub) = ctx_with_stub();
        let out = peer_create(
            PeerCreateArgs {
                action: "join".into(),
                addr: Some("host.local".into()),
                port: None,
                code: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.action, "join");
        assert_eq!(out.inviter_addr.as_deref(), Some("host.local"));
        let g = stub.last_join.lock().unwrap();
        assert_eq!(g.as_ref().unwrap(), &("host.local".to_string(), None));
    }

    #[tokio::test]
    async fn peer_create_rejects_unknown_action() {
        let (ctx, _) = ctx_with_stub();
        let e = peer_create(
            PeerCreateArgs {
                action: "bogus".into(),
                addr: None,
                port: None,
                code: None,
            },
            &ctx,
        )
        .await
        .err()
        .unwrap();
        assert!(e.to_string().contains("unknown action"));
    }

    #[tokio::test]
    async fn pod_leave_forwards_peer() {
        let (ctx, stub) = ctx_with_stub();
        let out = pod_peer_delete(
            PodLeaveArgs {
                peer_id: "peer.l".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.rows_removed, 2);
        assert_eq!(
            stub.last_leave_peer.lock().unwrap().as_deref(),
            Some("peer.l")
        );
    }

    #[tokio::test]
    async fn pod_cert_status_passthrough() {
        let (ctx, _) = ctx_with_stub();
        let out = pod_detail(EmptyArgs {}, &ctx).await.unwrap();
        assert!(out.founder);
        assert!(out.member);
        assert!(!out.self_secure);
    }

    #[tokio::test]
    async fn pod_detail_reflects_self_secure() {
        let (ctx, stub) = ctx_with_stub();
        *stub.self_secure.lock().unwrap() = true;
        let out = pod_detail(EmptyArgs {}, &ctx).await.unwrap();
        assert!(out.self_secure);
    }

    #[tokio::test]
    async fn pod_update_sets_self_secure() {
        let (ctx, stub) = ctx_with_stub();
        let out = pod_update(
            PodUpdateArgs {
                self_secure: Some(true),
                peer_id: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.self_secure);
        assert!(*stub.self_secure.lock().unwrap());
    }

    #[tokio::test]
    async fn pod_update_none_is_read_only() {
        let (ctx, stub) = ctx_with_stub();
        *stub.self_secure.lock().unwrap() = true;
        let out = pod_update(
            PodUpdateArgs {
                self_secure: None,
                peer_id: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.self_secure);
        // unchanged
        assert!(*stub.self_secure.lock().unwrap());
    }

    #[tokio::test]
    async fn exec_dispatch_records_peer_tool_and_args() {
        let stub = StubPod::default();
        let out = stub
            .exec("peer.x", "tool.y", serde_json::json!({"k": "v"}))
            .await
            .unwrap();
        assert_eq!(out.peer, "peer.x");
        assert_eq!(out.tool, "tool.y");
        assert_eq!(out.result, serde_json::json!({"ok": true}));
        let g = stub.last_exec.lock().unwrap();
        let (p, t, a) = g.as_ref().unwrap();
        assert_eq!(p, "peer.x");
        assert_eq!(t, "tool.y");
        assert_eq!(a, &serde_json::json!({"k": "v"}));
    }

    struct DummyProvider(Arc<StubPod>);
    impl ProvidePod for DummyProvider {
        fn pod(&self) -> Arc<dyn PodService> {
            self.0.clone()
        }
    }

    #[test]
    fn register_pod_installs_service_into_ctx() {
        let mut ctx = empty_ctx();
        let stub = Arc::new(StubPod::default());
        register_pod(&mut ctx, &DummyProvider(stub));
        // svc() resolves only if register_pod actually installed it.
        let _svc = native_support::svc(&ctx).expect("service registered");
    }

    #[tokio::test]
    async fn svc_errors_when_service_not_registered() {
        let ctx = empty_ctx();
        let err = pod_peer_list(EmptyArgs {}, &ctx).await.err();
        assert!(err.is_some(), "expected error when PodService missing");
    }
}
