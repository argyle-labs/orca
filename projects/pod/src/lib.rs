//! Pod / mesh tools surfaced to every tool surface (CLI + REST + MCP).
//!
//! `pod.list` mirrors the CLI's `orca pod list` so the web overview can
//! render paired peers without a bespoke REST endpoint. The mesh ops need
//! mTLS dials, PKI material, and bootstrap signing — all of which live in
//! `crate::native` + `crate::server_pod` within this crate.
//!
//! Tools call `crate::server_pod::*` free fns directly — no service trait
//! (dissolved in slice 4 per [[feedback_no_indirection]]). The daemon only
//! registers a `PodRemoteExec` transport so orca-dispatch can route
//! `remote_ok` tools to peers.

pub mod cli;
pub mod host_status_writer;
pub mod native;
pub mod server_pod;

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
    pub system: Option<system::system_info_types::SystemInfoReport>,
}

/// Unified pod-membership view. Every row carries a `state` discriminant so
/// callers see joined members, in-flight handshakes, and mDNS-discovered
/// candidates in one shape. Replaces the previous trio of `system.peer.list`,
/// `system.peer.discovery.list`, and `system.peer.handshake.list` (2026-05-28
/// consolidation — see project_pod_peer_system_consolidation.md).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum PodMember {
    /// Paired pod member — full mTLS peer with addressing, runtime info, and
    /// (when probed) ping latency + system snapshot. Boxed because the joined
    /// row carries an optional `SystemInfoReport` that's ~1 KB larger than
    /// the other variants; without the indirection the whole enum pays that
    /// size on every row.
    Joined(Box<PodPeerDto>),
    /// Pending inbound or outbound offer — pairing handshake in progress.
    Handshaking(PodPendingOfferDto),
    /// mDNS-discovered orca that is not yet paired.
    Discovered(PodDiscoveryRowDto),
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodListOutput {
    pub members: Vec<PodMember>,
}

// ── pod.join — unified pairing entry point ───────────────────────────────────
//
// `action` selects the pairing role:
//   "invite"  — inviter pushes offer to a discovered joiner  (needs `addr`)
//   "join"    — joiner pulls offer from an out-of-mDNS host  (needs `addr`)
//   "accept"  — joiner accepts a pending inbound offer        (needs `code`)

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinArgs {
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

/// Output for `pod.join`, tagged by the pairing `action`. Each variant carries
/// exactly the fields its role produces — no cross-variant `Option` soup.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum PodJoinOutput {
    /// Inviter pushed an offer to a discovered joiner.
    Invite {
        pairing_code: String,
        joiner_hostname: String,
        joiner_addr: String,
        joiner_port: u16,
        joiner_pubkey_fp: String,
        offer_id: String,
        expires_at: i64,
    },
    /// Joiner requested an offer from an out-of-mDNS inviter.
    Join {
        pairing_code: String,
        inviter_addr: String,
        inviter_port: u16,
    },
    /// Joiner accepted a pending inbound offer; pod membership established.
    Accept {
        pod_id: String,
        inviter_peer_id: String,
        inviter_hostname: String,
        inviter_addr: String,
        inviter_port: u16,
        self_secure: bool,
    },
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
    /// mDNS-advertised membership: `"unclaimed"` or `"pod:<pod_id>"`. Named
    /// `discovery_state` (not `state`) so it doesn't collide with the
    /// `#[serde(tag = "state")]` discriminant on [`PodMember`], which would
    /// otherwise clobber the `"discovered"` tag and break state filtering.
    pub discovery_state: String,
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

// ── pod.join "join" sub-action — internal types ──────────────────────────────
//
// Used by the `pod.join` tool when `action="join"`: the joiner pulls an offer
// from an inviter not yet in mDNS. Renamed from PodJoinArgs/Output (2026-05-28)
// because the user-facing umbrella tool now owns the `PodJoin*` names.

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinRequestArgs {
    /// Inviter's address (host or host:port).
    pub inviter_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodJoinRequestOutput {
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

// ── pod.leave (voluntary self exit) ──────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveSelfResult {
    pub peer_id: String,
    pub notify_result: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodLeaveSelfOutput {
    /// Number of peer rows removed from `pod_peers` (one per paired peer).
    pub rows_removed: u32,
    pub peers: Vec<PodLeaveSelfResult>,
}

// ── pod.recover ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodRecoverArgs {
    /// Peer whose stale `departed_at` flag should be cleared on THIS host.
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodRecoverOutput {
    pub peer_id: String,
    /// `true` if a `departed_at` flag was actually cleared. `false` means the
    /// peer either wasn't departed or doesn't exist locally.
    pub cleared: bool,
}

// ── pod.forget ───────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodForgetArgs {
    /// Stale/orphan peer_id to purge mesh-wide (e.g. an old identity left over
    /// from a machine_id change, or a decommissioned host).
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodForgetNotice {
    /// A live member we asked to forget the target.
    pub peer_id: String,
    /// `"notified"` or `"warn: <err>"`.
    pub result: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodForgetOutput {
    pub peer_id: String,
    /// Rows deleted on THIS host across pod_peers/pod_trust/pod_discovery/offers.
    pub rows_removed: u32,
    /// Per-member fan-out result.
    pub notified: Vec<PodForgetNotice>,
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

// ── DTO conversions + wire-dispatch types ───────────────────────────────────

mod dto_conversions {
    use super::*;

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
}

/// Internal-only envelope for [`server_pod::exec`]. JSON `Value` here is the
/// JSON-RPC wire payload — type-erased only because the peer-side registry
/// dispatches by name. Callers go through [`crate::cli::exec_remote`], which
/// deserializes into the typed `OrcaToolDef::Output` immediately on receipt,
/// so no opaque value ever reaches a user-facing type.
#[allow(clippy::disallowed_types)]
pub struct PodExecDispatch {
    pub peer: String,
    pub tool: String,
    pub result: serde_json::Value,
}

/// Transport that lets the generic `orca_contract::RemoteExec` trait dispatch
/// through `server_pod::exec`. Registered in the daemon's `build_tool_ctx` so
/// `cli::exec_remote::<T>(...)` (in orca-dispatch, which knows nothing about
/// pod) finds a peer transport. Unit struct — no service indirection.
#[cfg(feature = "cli")]
pub struct PodRemoteExec;

#[cfg(feature = "cli")]
#[async_trait::async_trait]
impl orca_contract::RemoteExec for PodRemoteExec {
    #[allow(clippy::disallowed_types)]
    async fn exec(
        &self,
        peer: &str,
        tool: &str,
        args: serde_json::Value,
        caller: Option<orca_contract::CallerIdentity>,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(server_pod::exec(peer, tool, args, caller).await?.result)
    }
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// Unified pod-membership view: joined members + in-flight handshakes +
/// mDNS-discovered candidates, each row tagged by `state`. Replaces the trio
/// of `system.peer.list`, `system.peer.discovery.list`, and
/// `system.peer.handshake.list` (2026-05-28 consolidation).
#[orca_tool(domain = "pod", verb = "list")]
async fn pod_list(
    _args: EmptyArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodListOutput> {
    let joined = server_pod::list_enriched().await?;
    let handshaking = server_pod::pending().unwrap_or_default();
    let discovered = server_pod::discover().unwrap_or_default();

    // Dedup the mDNS discovery layer against the membership layer. A peer that
    // is already joined keeps re-advertising over mDNS as `unclaimed.<mid>`;
    // without this filter every joined host also shows a phantom "unclaimed"
    // row. Collapse `peer.<mid>` and `unclaimed.<mid>` to the shared `<mid>`
    // key, and drop our own self-sighting.
    fn machine_key(peer_id: &str) -> &str {
        peer_id.split_once('.').map_or(peer_id, |(_, mid)| mid)
    }
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    claimed.insert(system::host_identity::machine_id_short().to_string());
    for p in &joined {
        claimed.insert(machine_key(&p.peer_id).to_string());
    }
    let discovered: Vec<_> = discovered
        .into_iter()
        .filter(|d| {
            d.peer_id
                .as_deref()
                .is_none_or(|pid| !claimed.contains(machine_key(pid)))
        })
        .collect();

    let mut members = Vec::with_capacity(joined.len() + handshaking.len() + discovered.len());
    members.extend(joined.into_iter().map(|p| PodMember::Joined(Box::new(p))));
    members.extend(handshaking.into_iter().map(PodMember::Handshaking));
    members.extend(discovered.into_iter().map(PodMember::Discovered));
    Ok(PodListOutput { members })
}

/// Initiate or complete a pod-membership pairing.
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
#[orca_tool(domain = "pod", verb = "join")]
async fn pod_join(
    args: PodJoinArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodJoinOutput> {
    match args.action.as_str() {
        "invite" => {
            let addr = args
                .addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("invite requires addr"))?;
            let out = server_pod::offer(addr, args.port).await?;
            Ok(PodJoinOutput::Invite {
                pairing_code: out.code,
                joiner_hostname: out.joiner_hostname,
                joiner_addr: out.joiner_addr,
                joiner_port: out.joiner_port,
                joiner_pubkey_fp: out.joiner_pubkey_fp,
                offer_id: out.offer_id,
                expires_at: out.expires_at,
            })
        }
        "join" => {
            let addr = args
                .addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("join requires addr"))?;
            let out = server_pod::join(addr, args.port).await?;
            Ok(PodJoinOutput::Join {
                pairing_code: out.code,
                inviter_addr: out.inviter_addr,
                inviter_port: out.inviter_port,
            })
        }
        "accept" => {
            let code = args
                .code
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("accept requires code"))?;
            let out = server_pod::accept(code).await?;
            Ok(PodJoinOutput::Accept {
                pod_id: out.pod_id,
                inviter_peer_id: out.inviter_peer_id,
                inviter_hostname: out.inviter_hostname,
                inviter_addr: out.inviter_addr,
                inviter_port: out.inviter_port,
                self_secure: out.self_secure,
            })
        }
        other => anyhow::bail!("unknown action '{other}' (expected invite|join|accept)"),
    }
}

/// Set trust for a paired peer. Without `push`, mutates OUR local trust
/// (`local_secure`). With `push: true`, executes on the remote peer over
/// mTLS so THEY trust US (`peer_secure` from our perspective).
#[orca_tool(domain = "pod", verb = "trust")]
async fn pod_trust(
    args: PodTrustArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodTrustOutput> {
    if args.push {
        return server_pod::push_trust(&args.peer_id, args.on, ctx.caller()).await;
    }
    server_pod::trust(&args.peer_id, args.on).await
}

/// mTLS ping a paired peer; returns latency + their self-reported identity.
/// Kept distinct from `system.detail --peer <id>` because ping latency is a
/// *relationship* measurement between this host and the peer, not a property
/// of the peer itself.
#[orca_tool(domain = "pod", verb = "ping")]
async fn pod_ping(
    args: PodPingArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodPingOutput> {
    Ok(server_pod::ping(&args.peer_id).await)
}

/// Evict a paired peer: best-effort notify, then drop `pod_peers` + `pod_trust`
/// rows for it. Mirrors today's `system.peer.delete` semantics.
#[orca_tool(domain = "pod", verb = "kick", role = "admin")]
async fn pod_kick(
    args: PodLeaveArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodLeaveOutput> {
    server_pod::leave_peer(&args.peer_id).await
}

/// Voluntary pod exit: notify every paired peer we're leaving (best-effort),
/// then drop all `pod_peers` + `pod_trust` rows on this host. PKI material is
/// left in place — call `system bootstrap` to fully reset.
#[orca_tool(domain = "pod", verb = "leave", role = "admin", local_only = true)]
async fn pod_leave(
    _args: EmptyArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodLeaveSelfOutput> {
    server_pod::leave_self().await
}

/// Clear a stale `departed_at` flag for a peer on THIS host. Recovery tool
/// for the 2026-05-28 kick/peer-leaving bug (and any future false-depart).
/// No network call — purely local row repair.
#[orca_tool(domain = "pod", verb = "recover", role = "admin", local_only = true)]
async fn pod_recover(
    args: PodRecoverArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodRecoverOutput> {
    server_pod::recover(&args.peer_id)
}

/// Forget a stale/orphan peer_id mesh-wide: hard-delete it here AND fan a
/// one-way notice to every live member so they drop it too. Use for orphans
/// left by machine_id churn or decommissioned hosts — NOT for evicting a live
/// peer (that's `pod kick`).
#[orca_tool(domain = "pod", verb = "forget", role = "admin")]
async fn pod_forget(
    args: PodForgetArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodForgetOutput> {
    server_pod::forget(&args.peer_id).await
}

/// Days-remaining + rotation state for every mesh cert on this host, plus
/// the current `self_secure` (Tier-2 secrets-storage) setting.
#[orca_tool(domain = "system.pod", verb = "detail")]
async fn pod_detail(
    _args: EmptyArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodCertStatusOutput> {
    server_pod::status()
}

/// Update pod-level settings on this host or — when `peer_id` is set —
/// on the named remote peer over the pod mesh. Currently exposes
/// `self_secure` (Tier-2 secrets-storage permission). Admin-only because
/// flipping it can authorize secrets replication into this host.
#[orca_tool(domain = "system.pod", verb = "update", role = "admin")]
async fn pod_update(
    args: PodUpdateArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PodUpdateOutput> {
    if let Some(ref peer_id) = args.peer_id {
        let dispatch = server_pod::exec(
            peer_id,
            "system.pod.update",
            serde_json::json!({ "self_secure": args.self_secure }),
            ctx.caller(),
        )
        .await?;
        return Ok(serde_json::from_value(dispatch.result)?);
    }
    let self_secure = match args.self_secure {
        Some(v) => server_pod::set_self_secure(v).await?,
        None => server_pod::get_self_secure()?,
    };
    Ok(PodUpdateOutput { self_secure })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_peer_address_from_db_row() {
        let row = db::host_addressing::PodPeerAddress {
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
        let row = db::pod::PeerSummary {
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
}
