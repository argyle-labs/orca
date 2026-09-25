//! Generic media tool surface.
//!
//! Media is a capability domain, not a plugin: every media app registers, per
//! media *type* (movies/tv/music/podcasts/audiobooks/ebooks/comics), as an
//! acquirer (`downloaded_by`) and/or a server (`served_by`). These verbs iterate
//! the process-global `media` registry ([`plugin_toolkit::media`]) rather than
//! naming any app by name:
//!
//! served_by/downloaded_by is one RELATION at TWO levels, and each level gets
//! exactly one verb:
//!
//! * `media.list`         — every registered (app × media-type × role) provider
//! * `media.detail`       — LEVEL 1, the media type as an entity: what *can*
//!   acquire and *can* serve it. Both halves of the relation on one object, with
//!   reachable URL and (with `--user`) the per-user credentials for device setup
//! * `media.unit.list`    — LEVEL 2, the CONVERGENCE view: canonical media units
//!   merged across every backend (variants/resolutions, subtitle tracks, file
//!   locations, every source — app stream and/or raw file over SMB/NFS) each
//!   carrying who actually got it and who actually holds it
//! * `media.unit.detail`  — LEVEL 2 for one unit, addressed by external id
//!
//! Level 2 is NOT derivable from level 1 (jellyfin *can* serve tv ≠ jellyfin
//! *has* this episode), so the unit fan-out tags each partial with the backend
//! that produced it and [`media::merge_units`] unions those contributors.
//!
//! N media plugins add 0 tools. Dispatched through the single daemon handler so
//! CLI / REST / MCP / UI share one path.

use derive::orca_tool;
use plugin_toolkit::media::{
    self, Capability, MediaCredentials, MediaRole, MediaType, MediaUnit, MediaUrl, Provider,
    merge_units,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Parse the `--media-type` arg string into the domain enum (wire strings match
/// `MediaType`'s snake_case serde rename). Kept local to the tool surface so the
/// media crate's registration-side parser stays `in-process`-gated.
fn parse_media_type(s: &str) -> anyhow::Result<MediaType> {
    match s {
        "movies" => Ok(MediaType::Movies),
        "tv" => Ok(MediaType::Tv),
        "music" => Ok(MediaType::Music),
        "podcasts" => Ok(MediaType::Podcasts),
        "audiobooks" => Ok(MediaType::Audiobooks),
        "ebooks" => Ok(MediaType::Ebooks),
        "comics" => Ok(MediaType::Comics),
        other => Err(anyhow::anyhow!(
            "unknown media type `{other}` (expected one of: movies, tv, music, podcasts, audiobooks, ebooks, comics)"
        )),
    }
}

// ── list ─────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaListArgs {
    /// Filter to one media type (movies/tv/music/podcasts/audiobooks/ebooks/comics).
    #[arg(long)]
    pub media_type: Option<String>,
    /// Filter to one role (`downloaded_by` / `served_by`).
    #[arg(long)]
    pub role: Option<String>,
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaListOutput {
    pub providers: Vec<Provider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Every registered media provider (app × media-type × role) and its
/// capabilities. Empty before any media plugin has bootstrapped.
#[orca_tool(domain = "media", verb = "list")]
async fn media_list(
    args: MediaListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MediaListOutput> {
    let want_type = match args.media_type.as_deref() {
        Some(s) => Some(parse_media_type(s)?),
        None => None,
    };
    let want_role = match args.role.as_deref() {
        Some("downloaded_by") => Some(MediaRole::DownloadedBy),
        Some("served_by") => Some(MediaRole::ServedBy),
        Some(other) => {
            return Err(anyhow::anyhow!(
                "unknown role `{other}` (expected `downloaded_by` or `served_by`)"
            ));
        }
        None => None,
    };
    let mut providers: Vec<Provider> = media::providers()
        .into_iter()
        .filter(|p| want_type.is_none_or(|t| p.media_type == t))
        .filter(|p| want_role.is_none_or(|r| p.roles.contains(&r)))
        .collect();
    providers.sort_by(|a, b| {
        a.media_type
            .as_str()
            .cmp(b.media_type.as_str())
            .then_with(|| a.name.cmp(&b.name))
    });
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(providers, &params);
    Ok(MediaListOutput {
        providers: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

// ── detail (level 1: the media type) ───────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaDetailArgs {
    /// Media type to resolve the relation for.
    #[arg(long)]
    pub media_type: String,
    /// Resolve reachable URL + orca-managed credentials for this user, so a device
    /// can be set up. Omit to just list the servers.
    #[arg(long)]
    pub user: Option<String>,
}

/// One server for a media type, with the reachable URL and (when `--user` was
/// given) that user's orca-managed credentials for device setup.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ServedByEntry {
    pub provider: Provider,
    /// Reachable URL(s), resolved when the backend supports the `url` capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<MediaUrl>,
    /// Per-user credentials, resolved when `--user` was given and the backend
    /// supports the `credentials` capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<MediaCredentials>,
    /// Non-fatal per-backend resolution error, if url/credentials couldn't resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaDetailOutput {
    pub media_type: String,
    /// Acquirers registered for this type — capability, not fact. Empty is a
    /// legitimate answer (nothing registered yet), never an error.
    pub downloaded_by: Vec<Provider>,
    /// Servers registered for this type. Multi-provider is normal (tv is served
    /// by BOTH plex and jellyfin).
    pub served_by: Vec<ServedByEntry>,
}

/// The media TYPE as an entity: both halves of the served-by/downloaded-by
/// relation at the registration level — what *can* acquire and what *can* serve
/// it, each server with its reachable URL and, with `--user`, the orca-managed
/// per-user credentials to set up a device.
/// `media detail --media-type audiobooks --user skey` returns skey's username +
/// URL for Audiobookshelf alongside the acquirers that feed it.
#[orca_tool(domain = "media", verb = "detail")]
async fn media_detail(
    args: MediaDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MediaDetailOutput> {
    let ty = parse_media_type(&args.media_type)?;
    let downloaded_by = media::downloaders_for(ty)
        .iter()
        .map(|b| b.provider())
        .collect();
    let mut servers = Vec::new();
    for b in media::servers_for(ty) {
        let mut entry = ServedByEntry {
            provider: b.provider(),
            url: None,
            credentials: None,
            error: None,
        };
        if b.supports(Capability::Url) {
            match b.url().await {
                Ok(u) => entry.url = Some(u),
                Err(e) => entry.error = Some(e.to_string()),
            }
        }
        if let Some(user) = args.user.as_deref()
            && b.supports(Capability::Credentials)
        {
            match b.credentials(user).await {
                Ok(c) => entry.credentials = Some(c),
                Err(e) => {
                    entry.error = Some(
                        entry
                            .error
                            .map_or_else(|| e.to_string(), |p| format!("{p}; {e}")),
                    );
                }
            }
        }
        servers.push(entry);
    }
    Ok(MediaDetailOutput {
        media_type: ty.as_str().to_string(),
        downloaded_by,
        served_by: servers,
    })
}

// ── unit convergence ─────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaUnitListArgs {
    /// Filter to one media type.
    #[arg(long)]
    pub media_type: Option<String>,
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaUnitListOutput {
    /// Canonical media units, merged across every backend that contributes a view.
    pub units: Vec<MediaUnit>,
    /// Non-fatal per-backend errors gathering unit views.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub errors: Vec<MediaUnitError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaUnitError {
    pub provider: String,
    pub error: String,
}

/// Fan out over every backend contributing a unit view, tag each partial with the
/// contributing backend by role, and merge by identity. The contributor is known
/// here and its role is already decidable from its registration, so level 2 of the
/// relation costs nothing extra — without the tag `merge_units` would flatten away
/// which backend produced which partial. A backend that fails is a non-fatal
/// `errors` row: one unreachable server must never fail the call.
async fn gather_units(want_type: Option<MediaType>) -> (Vec<MediaUnit>, Vec<MediaUnitError>) {
    let mut partials: Vec<MediaUnit> = Vec::new();
    let mut errors: Vec<MediaUnitError> = Vec::new();
    for b in media::backends() {
        if let Some(t) = want_type
            && b.media_type() != t
        {
            continue;
        }
        if !b.supports(Capability::Units) {
            continue;
        }
        match b.units().await {
            Ok(us) => {
                let roles = b.roles();
                let acquirer = roles.contains(&MediaRole::DownloadedBy);
                let server = roles.contains(&MediaRole::ServedBy);
                partials.extend(us.into_iter().map(|mut u| {
                    if acquirer {
                        u.downloaded_by.push(b.name().to_string());
                    }
                    if server {
                        u.served_by.push(b.name().to_string());
                    }
                    u
                }));
            }
            Err(e) => errors.push(MediaUnitError {
                provider: b.name().to_string(),
                error: e.to_string(),
            }),
        }
    }
    let mut units = merge_units(partials);
    if let Some(t) = want_type {
        units.retain(|u| u.media_type == t);
    }
    units.sort_by(|a, b| a.identity.title.cmp(&b.identity.title));
    (units, errors)
}

/// The convergence view: every canonical media UNIT, merged across all backends
/// that contribute a unit view (`units` capability). One unit collapses a work's
/// variants (resolutions/formats/subtitle tracks), its file locations, every way
/// it is served (app stream and/or raw file over SMB/NFS) and who actually got
/// and holds it into a single holistic object.
#[orca_tool(domain = "media.unit", verb = "list")]
async fn media_unit_list(
    args: MediaUnitListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MediaUnitListOutput> {
    let want_type = match args.media_type.as_deref() {
        Some(s) => Some(parse_media_type(s)?),
        None => None,
    };
    let (units, errors) = gather_units(want_type).await;
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(units, &params);
    Ok(MediaUnitListOutput {
        units: page.items,
        errors,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

// ── unit detail (level 2: the managed unit) ──────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaUnitDetailArgs {
    /// External id as `<source>:<id>` (`imdb:tt0133093`, `asin:B0785PYZQ4`) — the
    /// pair the merge indexes on. There is no synthetic stable unit id.
    #[arg(long)]
    pub id: String,
    /// Narrow the fan-out to one media type. Optional; also disambiguates an id
    /// reused across types.
    #[arg(long)]
    pub media_type: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaUnitDetailOutput {
    pub unit: MediaUnit,
    /// Non-fatal per-backend errors gathering unit views — the unit may be a
    /// partial picture if a server was unreachable.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub errors: Vec<MediaUnitError>,
}

/// One managed media UNIT addressed by external id, carrying its own resolved
/// `downloadedBy` / `servedBy` — the concrete fact ("where did THIS book come
/// from, where can I play it"), which the type-level relation cannot answer.
#[orca_tool(domain = "media.unit", verb = "detail")]
async fn media_unit_detail(
    args: MediaUnitDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MediaUnitDetailOutput> {
    let (source, id) = args.id.split_once(':').ok_or_else(|| {
        anyhow::anyhow!(
            "`--id` must be `<source>:<id>` (e.g. `imdb:tt0133093`), got `{}`",
            args.id
        )
    })?;
    // Canonicalize the same way the merge did, or a caller's `IMDB/0133093` would
    // never match the unit keyed under `imdb/tt0133093`.
    let want = media::identity::canonicalize(&media::ExternalId {
        source: source.to_string(),
        id: id.to_string(),
    });
    let want_type = match args.media_type.as_deref() {
        Some(s) => Some(parse_media_type(s)?),
        None => None,
    };
    let (units, errors) = gather_units(want_type).await;
    let mut matches: Vec<MediaUnit> = units
        .into_iter()
        .filter(|u| u.identity.external_ids.contains(&want))
        .collect();
    // Ambiguity is reported, never silently resolved by picking the first.
    if matches.len() > 1 {
        let candidates = matches
            .iter()
            .map(|u| {
                format!(
                    "{} ({}{})",
                    u.identity.title,
                    u.media_type.as_str(),
                    u.identity
                        .year
                        .map_or_else(String::new, |y| format!(", {y}"))
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(anyhow::anyhow!(
            "`{}` matches {} units — narrow with `--media-type`. Candidates: {candidates}",
            args.id,
            matches.len()
        ));
    }
    let unit = matches.pop().ok_or_else(|| {
        anyhow::anyhow!(
            "no media unit carries external id `{}:{}`",
            want.source,
            want.id
        )
    })?;
    Ok(MediaUnitDetailOutput { unit, errors })
}
