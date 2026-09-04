//! Generic media tool surface.
//!
//! Media is a capability domain, not a plugin: every media app registers, per
//! media *type* (movies/tv/music/podcasts/audiobooks/ebooks/comics), as an
//! acquirer (`downloaded_by`) and/or a server (`served_by`). These verbs iterate
//! the process-global `media` registry ([`plugin_toolkit::media`]) rather than
//! naming any app by name:
//!
//! * `media.list`           — every registered (app × media-type × role) provider
//! * `media.downloaded-by`  — the acquirer(s) for a media type
//! * `media.served-by`      — the server(s) for a media type, with reachable URL
//!   and (with `--user`) the orca-managed per-user credentials to set up a device
//!
//! N media plugins add 0 tools. Dispatched through the single daemon handler so
//! CLI / REST / MCP / UI share one path.

use derive::orca_tool;
use plugin_toolkit::media::{self, MediaCredentials, MediaRole, MediaType, MediaUrl, Provider};
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

// ── downloaded-by ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaDownloadedByArgs {
    /// Media type to resolve the acquirer(s) for.
    #[arg(long)]
    pub media_type: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaDownloadedByOutput {
    pub media_type: String,
    /// The acquirer providers registered for this media type.
    pub providers: Vec<Provider>,
}

/// The acquirer(s) that download a given media type (`sonarr` for tv, …).
#[orca_tool(domain = "media", verb = "downloaded-by")]
async fn media_downloaded_by(
    args: MediaDownloadedByArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MediaDownloadedByOutput> {
    let ty = parse_media_type(&args.media_type)?;
    let providers = media::downloaders_for(ty)
        .iter()
        .map(|b| b.provider())
        .collect();
    Ok(MediaDownloadedByOutput {
        media_type: ty.as_str().to_string(),
        providers,
    })
}

// ── served-by ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaServedByArgs {
    /// Media type to resolve the server(s) for.
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
pub struct MediaServedByOutput {
    pub media_type: String,
    pub servers: Vec<ServedByEntry>,
}

/// The server(s) that serve a media type, each with its reachable URL and — with
/// `--user` — the orca-managed per-user credentials to set up a device. This is
/// the first served-by verb: `media served-by --media-type audiobooks --user skey`
/// returns skey's username + URL for Audiobookshelf.
#[orca_tool(domain = "media", verb = "served-by")]
async fn media_served_by(
    args: MediaServedByArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MediaServedByOutput> {
    use plugin_toolkit::media::Capability;
    let ty = parse_media_type(&args.media_type)?;
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
    Ok(MediaServedByOutput {
        media_type: ty.as_str().to_string(),
        servers,
    })
}
