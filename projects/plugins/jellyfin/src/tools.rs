//! Jellyfin tool surface.
//!
//! Endpoint registry: `jellyfin.{list, detail, create, update, delete}` —
//! generated wholesale by `endpoint_resource!`. The macro emits the row
//! struct, db helpers, schema fragment, args/output types, and the five
//! `#[orca_tool]`-annotated functions in one shot. See
//! [[feedback-plugin-toolkit-max-power-min-boilerplate]].
//!
//! Server diagnosis: `jellyfin.server_info`, `jellyfin.libraries`, and the
//! core `jellyfin.transcode_health` are hand-written `#[orca_tool]`s that
//! call out over HTTP through the typed `Client` rather than over the local
//! registry table.
//!
//! Endpoint resolution: every diagnosis tool accepts the endpoint *name* and
//! loads `(base_url, token)` from the toolkit-generated `endpoint_db` at call
//! time. Per [[project-colocated-api-clients]] + model B (any creds-holder may
//! execute), the row syncs to every paired peer so any of them can call
//! `jellyfin.*` against a registered endpoint.
//!
//! Imports flow through `plugin_toolkit::prelude::*` only — the plugin treats
//! the toolkit as the single gateway to the orca system.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;

use crate::diag::SessionTranscodeHealth;
use crate::{Client, Config, ServerInfo, VirtualFolder};

// ═══════════════════════════════════════════════════════════════════════════
// jellyfin.{list,detail,create,update,delete} — endpoint registry CRUD.
// One declaration → five tools, three transports each, schema fragment, db
// helpers, row struct, args/output types. Power scales with the macro.
// ═══════════════════════════════════════════════════════════════════════════

#[endpoint_resource(plugin = "jellyfin")]
pub struct JellyfinEndpoint {
    pub name: String,
    pub base_url: String,
    #[secret]
    pub token: String,
    pub enabled: bool,
}

// ── HTTP client helper ──────────────────────────────────────────────────────

fn make_client(name: &str) -> Result<Client> {
    let conn = runtime::open_db()?;
    let row = endpoint_db::get(&conn, name)?
        .with_context(|| format!("jellyfin endpoint '{name}' not registered"))?;
    if !row.enabled {
        bail!("jellyfin endpoint '{name}' is disabled");
    }
    Ok(Client::new(Config::new(row.base_url, row.token)))
}

// ═══════════════════════════════════════════════════════════════════════════
// jellyfin.server_info — server name / version / OS
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct JellyfinServerInfoArgs {
    /// Registered endpoint name.
    pub endpoint: String,
}

/// Server name, version, and operating system from `/System/Info`.
#[orca_tool(domain = "jellyfin", verb = "server_info")]
async fn jellyfin_server_info(args: JellyfinServerInfoArgs, _ctx: &ToolCtx) -> Result<ServerInfo> {
    Ok(make_client(&args.endpoint)?.server_info().await?)
}

// ═══════════════════════════════════════════════════════════════════════════
// jellyfin.libraries — configured libraries / virtual folders
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct JellyfinLibrariesArgs {
    /// Registered endpoint name.
    pub endpoint: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct JellyfinLibrariesOutput {
    /// Configured libraries from `/Library/VirtualFolders`.
    pub libraries: Vec<JellyfinLibrary>,
}

/// One configured library, flattened for the tool boundary.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JellyfinLibrary {
    pub name: Option<String>,
    pub collection_type: Option<String>,
    pub locations: Vec<String>,
    pub item_id: Option<String>,
}

impl From<VirtualFolder> for JellyfinLibrary {
    fn from(v: VirtualFolder) -> Self {
        Self {
            name: v.name,
            collection_type: v.collection_type,
            locations: v.locations.unwrap_or_default(),
            item_id: v.item_id,
        }
    }
}

/// Configured libraries (virtual folders) on a registered Jellyfin server.
#[orca_tool(domain = "jellyfin", verb = "libraries")]
async fn jellyfin_libraries(
    args: JellyfinLibrariesArgs,
    _ctx: &ToolCtx,
) -> Result<JellyfinLibrariesOutput> {
    let libraries = make_client(&args.endpoint)?
        .libraries()
        .await?
        .into_iter()
        .map(JellyfinLibrary::from)
        .collect();
    Ok(JellyfinLibrariesOutput { libraries })
}

// ═══════════════════════════════════════════════════════════════════════════
// jellyfin.transcode_health — CORE DIAGNOSIS
//
// `GET /Sessions` → per-session transcode state. A transcoding session whose
// `TranscodingInfo.HardwareAccelerationType` is `none` or absent is running a
// SOFTWARE transcode (CPU fallback) — the condition operators chase. The
// summary surfaces whether *any* session is software-fallback so a caller can
// branch without re-walking the list.
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct JellyfinTranscodeHealthArgs {
    /// Registered endpoint name.
    pub endpoint: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JellyfinTranscodeHealthOutput {
    /// Total active sessions reported by `/Sessions`.
    pub session_count: usize,
    /// Sessions actively transcoding (have a `TranscodingInfo`).
    pub transcoding_count: usize,
    /// Sessions transcoding on the CPU instead of a hardware encoder/decoder.
    pub software_fallback_count: usize,
    /// True when at least one session is a software fallback — the single
    /// flag a caller branches on to alert "HW accel is not engaging".
    pub any_software_fallback: bool,
    /// Per-session detail.
    pub sessions: Vec<SessionTranscodeHealth>,
}

/// **Core diagnosis.** Classify every active Jellyfin session as
/// direct-play, hardware transcode, or software (CPU) fallback, and flag
/// whether hardware acceleration is failing to engage.
#[orca_tool(domain = "jellyfin", verb = "transcode_health")]
async fn jellyfin_transcode_health(
    args: JellyfinTranscodeHealthArgs,
    _ctx: &ToolCtx,
) -> Result<JellyfinTranscodeHealthOutput> {
    let sessions = make_client(&args.endpoint)?.transcode_health().await?;
    let session_count = sessions.len();
    let transcoding_count = sessions.iter().filter(|s| s.is_transcoding).count();
    let software_fallback_count = sessions.iter().filter(|s| s.software_fallback).count();
    Ok(JellyfinTranscodeHealthOutput {
        session_count,
        transcoding_count,
        software_fallback_count,
        any_software_fallback: software_fallback_count > 0,
        sessions,
    })
}
