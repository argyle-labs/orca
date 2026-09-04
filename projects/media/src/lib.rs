//! Generic media domain. One model, one adapter trait, one registry — many
//! backends across many media types.
//!
//! Media is NOT a single plugin. It is a generic capability domain: every media
//! app is, per media *type* (movies / tv / music / podcasts / audiobooks /
//! ebooks / comics), either an **acquirer** (`downloaded_by` — sonarr, radarr,
//! lidarr, lazylibrarian, kapowarr) or a **server/player-backend** (`served_by`
//! — jellyfin, plex, navidrome, audiobookshelf, calibre, komga), or both. A
//! plugin registers what it downloads/serves and orca abstracts the two
//! capability shapes generically, so devices/users/controls are configured from
//! ONE place instead of per-app.
//!
//! Follows the same plug-in shape as `storage` / `containers`: a [`MediaBackend`]
//! trait + a process-global registry each adapter registers against at bootstrap.
//! A plugin declares one [`plugin_abi::BackendDef`] per (media_type) it handles
//! with `domain = "media"`, `kind` = the media type, and `capabilities` carrying
//! role markers (`downloaded_by` / `served_by`) plus the verbs it supports.

use derive::orca_async;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, RwLock};
use thiserror::Error;

// ── Domain model ─────────────────────────────────────────────────────────────

/// The media *type* axis — carried on `BackendDef::kind`. This is the primary
/// axis the aggregation surface groups by ("audiobooks downloaded-by / served-by").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Movies,
    Tv,
    Music,
    Podcasts,
    Audiobooks,
    Ebooks,
    Comics,
}

impl MediaType {
    /// Canonical wire string (matches `#[serde(rename_all = "snake_case")]`).
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaType::Movies => "movies",
            MediaType::Tv => "tv",
            MediaType::Music => "music",
            MediaType::Podcasts => "podcasts",
            MediaType::Audiobooks => "audiobooks",
            MediaType::Ebooks => "ebooks",
            MediaType::Comics => "comics",
        }
    }
}

/// The role a backend plays *for a given media type* — the second axis of the
/// domain. A backend may play both (rare, e.g. plex serves and can request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaRole {
    /// Acquirer: searches for and downloads/queues this media type.
    DownloadedBy,
    /// Server/player-backend: serves/streams this media type to devices.
    ServedBy,
}

impl MediaRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaRole::DownloadedBy => "downloaded_by",
            MediaRole::ServedBy => "served_by",
        }
    }
}

/// Capability strings a backend advertises (domain-interpreted). Carries both the
/// role markers and the concrete verbs; the media crate parses them off
/// `BackendDef::capabilities`. Role markers (`DownloadedBy`/`ServedBy`) double as
/// capabilities so a backend's role set is derivable from `capabilities()` alone,
/// mirroring how the `web` domain rides existing `BackendDef` axes with no new
/// ABI field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    // Role markers.
    DownloadedBy,
    ServedBy,
    // served_by verbs.
    /// Return reachable base URL(s) for device setup.
    Url,
    /// Return per-user credentials (orca-managed) for device setup.
    Credentials,
    // Shared library verbs.
    /// Enumerate the backend's library for this media type.
    List,
    /// Free-text search (acquisition or library lookup).
    Search,
    /// Add an item to the library (queue a download / import a file).
    LibraryAdd,
    /// Remove an item from the library.
    LibraryRemove,
    /// Re-match / fix an item's metadata identity.
    FixMatch,
    /// Report acquisition/library status.
    Status,
    /// Contribute this backend's partial [`MediaUnit`] view (identity +
    /// representations + how it serves them) for cross-backend convergence.
    Units,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::DownloadedBy => "downloaded_by",
            Capability::ServedBy => "served_by",
            Capability::Url => "url",
            Capability::Credentials => "credentials",
            Capability::List => "list",
            Capability::Search => "search",
            Capability::LibraryAdd => "library_add",
            Capability::LibraryRemove => "library_remove",
            Capability::FixMatch => "fix_match",
            Capability::Status => "status",
            Capability::Units => "units",
        }
    }
}

/// A descriptor row for one registered media backend — the `media.*` list view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Provider {
    /// App identity (`audiobookshelf`, `lazylibrarian`, `plex`).
    pub name: String,
    /// The media type this registration handles.
    pub media_type: MediaType,
    /// Roles this backend plays for the type.
    pub roles: Vec<MediaRole>,
    /// Verbs advertised.
    pub capabilities: Vec<Capability>,
    /// Non-secret base endpoint for display (`http://10.0.0.6:13378`).
    pub endpoint: String,
}

/// A reachable URL for a served-by backend, for device setup. `primary` is the
/// best guess; `alternates` are additional reachable paths (LAN, tailscale, FQDN).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaUrl {
    pub primary: String,
    #[serde(default)]
    pub alternates: Vec<String>,
}

/// Per-user credentials for a served-by backend, so a device can be set up. The
/// password is a [`SecretRef`] the secrets domain resolves — the media backend
/// never inlines a plaintext secret. orca owns/propagates the actual value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaCredentials {
    /// The username orca manages for this user on this backend.
    pub username: String,
    /// Reference to the password secret (resolved via the secrets domain), if the
    /// backend uses a password. Never a plaintext value on the wire.
    #[serde(default)]
    pub password_ref: Option<String>,
    /// Reachable URL(s) to point the device at.
    pub url: MediaUrl,
}

/// A reference to a credential the secrets domain resolves (`onepassword://…`,
/// `bitwarden://…`, or a native id). Transparent newtype so the contract is
/// explicit about which fields are credential references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SecretRef(pub String);

/// One item in a media library / search result. Deliberately minimal + generic
/// across types; type-specific detail rides `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaItem {
    /// Backend-local id (library item id, download id).
    pub id: String,
    /// Display title.
    pub title: String,
    /// Owning-backend-defined status (`downloaded`/`wanted`/`missing`/…), free text.
    #[serde(default)]
    pub status: Option<String>,
}

/// Outcome of a library mutation (add/remove/fix-match).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaMutation {
    pub ok: bool,
    #[serde(default)]
    pub message: Option<String>,
}

// ── Media unit: the convergence object ───────────────────────────────────────
//
// A MediaUnit is the canonical identity of a piece of media plus EVERYTHING about
// it: what it is (identity), its concrete variants (representations — resolutions,
// formats, subtitle/audio tracks), where the bytes live (locations), and who
// serves it and how (servings — an app stream and/or a raw file over SMB/NFS).
// This is the convergence of the media-server, storage, and series layers into
// one holistic object. A movie + its subtitles is ONE unit; a 4K + a 1080p copy
// is ONE unit; the same title served by Plex AND over NFS is ONE unit.

/// One external identifier that canonicalizes the work. The MERGE KEY: two
/// representations sharing any `(source, id)` are the same unit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ExternalId {
    /// Id namespace (`tmdb` / `imdb` / `tvdb` / `musicbrainz` / `isbn` /
    /// `comicvine` / `audible_asin` / …).
    pub source: String,
    pub id: String,
}

/// A series/sequence position, tying a unit to the cross-format reading-order
/// convergence (e.g. Stormlight #1). Kept generic across media types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SeriesRef {
    pub name: String,
    #[serde(default)]
    pub sequence: Option<String>,
}

/// What a unit IS — the identity every representation resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaIdentity {
    pub title: String,
    #[serde(default)]
    pub year: Option<u16>,
    /// Canonical external ids — the cross-server/​cross-format merge keys.
    #[serde(default)]
    pub external_ids: Vec<ExternalId>,
    /// Series/sequence position, when part of a series.
    #[serde(default)]
    pub series: Option<SeriesRef>,
}

/// Kind of a media track within a representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
    /// Subtitles — part of the SAME unit as the video they accompany.
    Subtitle,
}

/// One track (video / audio / subtitle) of a representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Track {
    pub kind: TrackKind,
    /// BCP-47/ISO language tag when known (`en`, `es`).
    #[serde(default)]
    pub language: Option<String>,
    /// Embedded in the container vs a sidecar file (e.g. an external `.srt`).
    #[serde(default)]
    pub embedded: bool,
    /// Sidecar file location, when the track is a separate file.
    #[serde(default)]
    pub location: Option<Location>,
}

/// Where bytes physically live — a reference into the storage domain. The same
/// representation may exist in several locations (replicated across shares/hosts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Location {
    /// Storage-domain provider name backing this path (`nfs` / `smb`), when known.
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Host the path is realized on, when known.
    #[serde(default)]
    pub host: Option<String>,
    /// Absolute path to the file on that storage.
    pub path: String,
}

/// One concrete manifestation of a unit: a quality/format variant plus its tracks
/// and where it lives. A 4K copy and a 1080p copy are two representations of one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Representation {
    /// Quality/edition marker (`2160p` / `1080p` / `FLAC` / `epub` / `cbz`).
    #[serde(default)]
    pub quality: Option<String>,
    /// Container/format (`mkv` / `m4b` / `epub`).
    #[serde(default)]
    pub container: Option<String>,
    /// Tracks (video/audio/subtitle) — subtitles included here belong to this unit.
    #[serde(default)]
    pub tracks: Vec<Track>,
    /// Physical location(s) of this representation's bytes.
    #[serde(default)]
    pub locations: Vec<Location>,
}

/// How a unit is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServingMethod {
    /// Streamed/transcoded by a media-server app (Plex/Jellyfin/Navidrome/ABS/…).
    AppStream,
    /// Served as a raw file over a storage protocol (SMB/NFS) — the file share is
    /// itself a way this unit is served.
    FileShare,
}

/// One way a unit is served: by which server/share, over which method, at what URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Serving {
    pub method: ServingMethod,
    /// The server app name (`plex`) or storage backend name (`smb`) doing the serving.
    pub by: String,
    /// Stream URL or share URL (`smb://host/share/path`, `nfs://host:/export/path`).
    #[serde(default)]
    pub url: Option<String>,
}

/// THE convergence object: a canonical piece of media with everything known about
/// it — what it is, its representations (resolutions/formats/tracks), where the
/// bytes live, and who serves it and how. Assembled by merging each backend's
/// partial view (see [`merge_units`]) by [`MediaIdentity`] external ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaUnit {
    pub media_type: MediaType,
    pub identity: MediaIdentity,
    #[serde(default)]
    pub representations: Vec<Representation>,
    #[serde(default)]
    pub servings: Vec<Serving>,
}

/// Merge a flat list of partial units (each backend contributes its own view) into
/// canonical units. Two partials merge when they share ANY `(source, id)` external
/// id; representations and servings union. Falls back to `(media_type, title,
/// year)` when a partial carries no external id, so an un-matched item still forms
/// its own unit rather than vanishing. This is the aggregation seam the
/// `media.unit.*` tools and the topology view build on.
pub fn merge_units(partials: Vec<MediaUnit>) -> Vec<MediaUnit> {
    // Union-find over partials keyed by shared external ids, with a title/year
    // fallback bucket. Kept simple + allocation-light for typical library sizes.
    let mut units: Vec<MediaUnit> = Vec::new();
    // Index: external-id key -> unit index; and fallback key -> unit index.
    use std::collections::HashMap;
    let mut by_ext: HashMap<(String, String), usize> = HashMap::new();
    let mut by_fallback: HashMap<(MediaType, String, Option<u16>), usize> = HashMap::new();

    for p in partials {
        // Find an existing unit this partial belongs to.
        let mut target: Option<usize> = None;
        for e in &p.identity.external_ids {
            if let Some(&i) = by_ext.get(&(e.source.clone(), e.id.clone())) {
                target = Some(i);
                break;
            }
        }
        let fb_key = (
            p.media_type,
            p.identity.title.to_lowercase(),
            p.identity.year,
        );
        if target.is_none()
            && p.identity.external_ids.is_empty()
            && let Some(&i) = by_fallback.get(&fb_key)
        {
            target = Some(i);
        }

        let idx = match target {
            Some(i) => i,
            None => {
                units.push(MediaUnit {
                    media_type: p.media_type,
                    identity: p.identity.clone(),
                    representations: Vec::new(),
                    servings: Vec::new(),
                });
                units.len() - 1
            }
        };

        // Register this partial's keys against the chosen unit.
        for e in &p.identity.external_ids {
            by_ext.insert((e.source.clone(), e.id.clone()), idx);
        }
        by_fallback.entry(fb_key).or_insert(idx);

        // Union content into the unit.
        let u = &mut units[idx];
        // Enrich identity: keep the richer title/year, union external ids + series.
        if u.identity.year.is_none() {
            u.identity.year = p.identity.year;
        }
        if u.identity.series.is_none() {
            u.identity.series = p.identity.series.clone();
        }
        for e in p.identity.external_ids {
            if !u.identity.external_ids.contains(&e) {
                u.identity.external_ids.push(e);
            }
        }
        for r in p.representations {
            if !u.representations.contains(&r) {
                u.representations.push(r);
            }
        }
        for s in p.servings {
            if !u.servings.contains(&s) {
                u.servings.push(s);
            }
        }
    }
    units
}

/// Errors a media backend op can produce.
#[derive(Debug, Error)]
pub enum MediaError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("capability not supported by backend `{0}`: {1:?}")]
    Unsupported(String, Capability),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

// ── Adapter trait ─────────────────────────────────────────────────────────────

/// A media backend: one app's registration for one media type. Required
/// identity/descriptor accessors + capability-gated verb defaults that return
/// [`MediaError::Unsupported`] until a backend overrides them.
#[orca_async]
pub trait MediaBackend: Send + Sync {
    /// App identity (`audiobookshelf`). Registry key is `(name, media_type)`.
    fn name(&self) -> &str;
    /// The media type this registration handles.
    fn media_type(&self) -> MediaType;
    /// Capabilities advertised (includes the role markers).
    fn capabilities(&self) -> Vec<Capability>;
    /// Non-secret base endpoint for display.
    fn endpoint(&self) -> String;

    /// Roles derived from capabilities (the role markers).
    fn roles(&self) -> Vec<MediaRole> {
        let mut r = Vec::new();
        let caps = self.capabilities();
        if caps.contains(&Capability::DownloadedBy) {
            r.push(MediaRole::DownloadedBy);
        }
        if caps.contains(&Capability::ServedBy) {
            r.push(MediaRole::ServedBy);
        }
        r
    }

    fn supports(&self, cap: Capability) -> bool {
        self.capabilities().contains(&cap)
    }

    /// Descriptor row for the list view. Default builds it from the accessors.
    fn provider(&self) -> Provider {
        Provider {
            name: self.name().to_string(),
            media_type: self.media_type(),
            roles: self.roles(),
            capabilities: self.capabilities(),
            endpoint: self.endpoint(),
        }
    }

    // ── served_by verbs ──
    /// Reachable URL(s) for device setup.
    async fn url(&self) -> Result<MediaUrl, MediaError> {
        Err(MediaError::Unsupported(self.name().into(), Capability::Url))
    }
    /// Per-user credentials (orca-managed) for device setup.
    async fn credentials(&self, _user: &str) -> Result<MediaCredentials, MediaError> {
        Err(MediaError::Unsupported(
            self.name().into(),
            Capability::Credentials,
        ))
    }

    // ── library / acquisition verbs ──
    async fn list(&self) -> Result<Vec<MediaItem>, MediaError> {
        Err(MediaError::Unsupported(
            self.name().into(),
            Capability::List,
        ))
    }
    async fn search(&self, _query: &str) -> Result<Vec<MediaItem>, MediaError> {
        Err(MediaError::Unsupported(
            self.name().into(),
            Capability::Search,
        ))
    }
    async fn library_add(&self, _item_ref: &str) -> Result<MediaMutation, MediaError> {
        Err(MediaError::Unsupported(
            self.name().into(),
            Capability::LibraryAdd,
        ))
    }
    async fn library_remove(&self, _item_id: &str) -> Result<MediaMutation, MediaError> {
        Err(MediaError::Unsupported(
            self.name().into(),
            Capability::LibraryRemove,
        ))
    }
    async fn fix_match(
        &self,
        _item_id: &str,
        _target_ref: &str,
    ) -> Result<MediaMutation, MediaError> {
        Err(MediaError::Unsupported(
            self.name().into(),
            Capability::FixMatch,
        ))
    }

    /// This backend's partial [`MediaUnit`] view: the units it knows about, each
    /// carrying the identity, representations, and servings THIS backend can see.
    /// Core merges these across all backends (see [`merge_units`]) into canonical
    /// units. A media server contributes its stream servings + metadata identity;
    /// a storage backend would contribute file locations + a `FileShare` serving.
    async fn units(&self) -> Result<Vec<MediaUnit>, MediaError> {
        Err(MediaError::Unsupported(
            self.name().into(),
            Capability::Units,
        ))
    }
}

// ── Process-global registry ─────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn MediaBackend>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Registry key: an app is identified per media type, so `(name, media_type)` is
/// the identity — plex registers separately for movies, tv, and music.
fn same_identity(a: &dyn MediaBackend, name: &str, ty: MediaType) -> bool {
    a.name() == name && a.media_type() == ty
}

/// Register a media backend. Re-registering the same `(name, media_type)`
/// replaces the entry so a reload doesn't duplicate providers.
pub fn register_backend(backend: Arc<dyn MediaBackend>) {
    let mut g = GLOBAL.write().expect("media registry poisoned");
    let (name, ty) = (backend.name().to_string(), backend.media_type());
    if let Some(slot) = g.iter_mut().find(|b| same_identity(b.as_ref(), &name, ty)) {
        *slot = backend;
    } else {
        g.push(backend);
    }
}

/// Snapshot of every registered backend.
pub fn backends() -> Vec<Arc<dyn MediaBackend>> {
    GLOBAL.read().expect("media registry poisoned").clone()
}

/// Look up a single backend by `(name, media_type)`.
pub fn backend(name: &str, media_type: MediaType) -> Option<Arc<dyn MediaBackend>> {
    GLOBAL
        .read()
        .expect("media registry poisoned")
        .iter()
        .find(|b| same_identity(b.as_ref(), name, media_type))
        .cloned()
}

/// Deregister every backend registered under `name` (across all media types),
/// the unload path a plugin's domain-registration needs. Returns count removed.
pub fn deregister_backend(name: &str) -> usize {
    let mut g = GLOBAL.write().expect("media registry poisoned");
    let before = g.len();
    g.retain(|b| b.name() != name);
    before - g.len()
}

/// Descriptor rows for every registered provider — the `media.list` view.
pub fn providers() -> Vec<Provider> {
    backends().iter().map(|b| b.provider()).collect()
}

/// Backends that DOWNLOAD `media_type` (`media <type> downloaded-by`).
pub fn downloaders_for(media_type: MediaType) -> Vec<Arc<dyn MediaBackend>> {
    backends()
        .into_iter()
        .filter(|b| b.media_type() == media_type && b.roles().contains(&MediaRole::DownloadedBy))
        .collect()
}

/// Backends that SERVE `media_type` (`media <type> served-by`).
pub fn servers_for(media_type: MediaType) -> Vec<Arc<dyn MediaBackend>> {
    backends()
        .into_iter()
        .filter(|b| b.media_type() == media_type && b.roles().contains(&MediaRole::ServedBy))
        .collect()
}

// ── Host-side loaded-plugin proxy + FFI seam (in-process only) ────────────────

/// The synchronous `(op, args_json) -> result_json` closure the loader supplies,
/// bridging to the subprocess wire. A thin subprocess plugin links no loader path
/// and no tokio, so the whole proxy surface is gated out on thin builds.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> Result<String, MediaError> + Send + Sync + 'static>;

/// Build and register a [`MediaBackend`] from a plugin's backend descriptor plus
/// an [`InvokeThunk`]. The loader calls this from its domain dispatch table; it
/// parses `kind`/`capabilities` into the domain enums and wires every advertised
/// operation back through `invoke`. Unknown values are rejected at load so a typo
/// surfaces then, not at first use.
#[cfg(feature = "in-process")]
pub fn register_from_def(
    name: String,
    kind: &str,
    endpoint: String,
    capabilities: &[String],
    invoke: InvokeThunk,
) -> Result<(), MediaError> {
    let media_type = parse_media_type(kind)?;
    let capabilities = capabilities
        .iter()
        .map(|c| parse_capability(c))
        .collect::<Result<Vec<_>, _>>()?;
    register_backend(Arc::new(MediaProxy {
        name,
        media_type,
        endpoint,
        capabilities,
        invoke,
    }));
    Ok(())
}

#[cfg(feature = "in-process")]
fn parse_media_type(s: &str) -> Result<MediaType, MediaError> {
    match s {
        "movies" => Ok(MediaType::Movies),
        "tv" => Ok(MediaType::Tv),
        "music" => Ok(MediaType::Music),
        "podcasts" => Ok(MediaType::Podcasts),
        "audiobooks" => Ok(MediaType::Audiobooks),
        "ebooks" => Ok(MediaType::Ebooks),
        "comics" => Ok(MediaType::Comics),
        other => Err(MediaError::Other(format!("unknown media type `{other}`"))),
    }
}

#[cfg(feature = "in-process")]
fn parse_capability(s: &str) -> Result<Capability, MediaError> {
    match s {
        "downloaded_by" => Ok(Capability::DownloadedBy),
        "served_by" => Ok(Capability::ServedBy),
        "url" => Ok(Capability::Url),
        "credentials" => Ok(Capability::Credentials),
        "list" => Ok(Capability::List),
        "search" => Ok(Capability::Search),
        "library_add" => Ok(Capability::LibraryAdd),
        "library_remove" => Ok(Capability::LibraryRemove),
        "fix_match" => Ok(Capability::FixMatch),
        "status" => Ok(Capability::Status),
        "units" => Ok(Capability::Units),
        other => Err(MediaError::Other(format!(
            "unknown media capability `{other}`"
        ))),
    }
}

/// A [`MediaBackend`] backed by a subprocess plugin reached over the JSON-proxy
/// wire. Each async trait method serializes its args, offloads the synchronous
/// [`InvokeThunk`] onto `spawn_blocking`, and deserializes the JSON result.
#[cfg(feature = "in-process")]
struct MediaProxy {
    name: String,
    media_type: MediaType,
    endpoint: String,
    capabilities: Vec<Capability>,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl MediaProxy {
    async fn call<A, R>(&self, op: &'static str, args: A) -> Result<R, MediaError>
    where
        A: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let args_json = serde_json::to_string(&args)
            .map_err(|e| MediaError::Other(format!("encode `{op}` args: {e}")))?;
        let invoke = self.invoke.clone();
        let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
            .await
            .map_err(|e| MediaError::Transport(format!("`{op}` proxy task failed: {e}")))??;
        serde_json::from_str(&out)
            .map_err(|e| MediaError::Other(format!("decode `{op}` result: {e}")))
    }
}

#[cfg(feature = "in-process")]
#[orca_async]
impl MediaBackend for MediaProxy {
    fn name(&self) -> &str {
        &self.name
    }
    fn media_type(&self) -> MediaType {
        self.media_type
    }
    fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.clone()
    }
    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    async fn url(&self) -> Result<MediaUrl, MediaError> {
        self.call("url", ()).await
    }
    async fn credentials(&self, user: &str) -> Result<MediaCredentials, MediaError> {
        self.call(
            "credentials",
            CredentialsArgs {
                user: user.to_string(),
            },
        )
        .await
    }
    async fn list(&self) -> Result<Vec<MediaItem>, MediaError> {
        self.call("list", ()).await
    }
    async fn search(&self, query: &str) -> Result<Vec<MediaItem>, MediaError> {
        self.call(
            "search",
            SearchArgs {
                query: query.to_string(),
            },
        )
        .await
    }
    async fn library_add(&self, item_ref: &str) -> Result<MediaMutation, MediaError> {
        self.call(
            "library_add",
            ItemRefArg {
                item_ref: item_ref.to_string(),
            },
        )
        .await
    }
    async fn library_remove(&self, item_id: &str) -> Result<MediaMutation, MediaError> {
        self.call(
            "library_remove",
            ItemIdArg {
                item_id: item_id.to_string(),
            },
        )
        .await
    }
    async fn fix_match(
        &self,
        item_id: &str,
        target_ref: &str,
    ) -> Result<MediaMutation, MediaError> {
        self.call(
            "fix_match",
            FixMatchArgs {
                item_id: item_id.to_string(),
                target_ref: target_ref.to_string(),
            },
        )
        .await
    }
    async fn units(&self) -> Result<Vec<MediaUnit>, MediaError> {
        self.call("units", ()).await
    }
}

// ── Wire-arg structs (shared by proxy encode + dispatch decode) ───────────────

#[derive(Serialize, Deserialize)]
struct CredentialsArgs {
    user: String,
}
#[derive(Serialize, Deserialize)]
struct SearchArgs {
    query: String,
}
#[derive(Serialize, Deserialize)]
struct ItemRefArg {
    item_ref: String,
}
#[derive(Serialize, Deserialize)]
struct ItemIdArg {
    item_id: String,
}
#[derive(Serialize, Deserialize)]
struct FixMatchArgs {
    item_id: String,
    target_ref: String,
}

/// Plugin-side inverse of [`MediaProxy`]: decode a proxied op's JSON args and
/// route it to an in-process [`MediaBackend`], returning the op's JSON result (or
/// an error string). Both halves of the FFI boundary live here so the wire
/// contract has a single source of truth — a backend plugin's `invoke` is one
/// call to this function. `op` is the bare operation name (the loader's thunk
/// strips the invoke prefix first). Always compiled (a thin plugin needs it).
#[allow(clippy::disallowed_types)] // erased-invoke dispatch seam — Value in/out.
pub async fn dispatch_op(
    backend: &dyn MediaBackend,
    op: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    fn enc<T: Serialize>(value: &T) -> Result<serde_json::Value, serde_json::Value> {
        serde_json::to_value(value)
            .map_err(|e| serde_json::Value::String(format!("failed to encode result: {e}")))
    }
    fn dec<T: serde::de::DeserializeOwned>(
        op: &str,
        args: serde_json::Value,
    ) -> Result<T, serde_json::Value> {
        serde_json::from_value(args)
            .map_err(|e| serde_json::Value::String(format!("invalid `{op}` args: {e}")))
    }
    fn err<E: std::fmt::Display>(e: E) -> serde_json::Value {
        serde_json::Value::String(e.to_string())
    }

    match op {
        "url" => enc(&backend.url().await.map_err(err)?),
        "credentials" => {
            let a: CredentialsArgs = dec(op, args)?;
            enc(&backend.credentials(&a.user).await.map_err(err)?)
        }
        "list" => enc(&backend.list().await.map_err(err)?),
        "search" => {
            let a: SearchArgs = dec(op, args)?;
            enc(&backend.search(&a.query).await.map_err(err)?)
        }
        "library_add" => {
            let a: ItemRefArg = dec(op, args)?;
            enc(&backend.library_add(&a.item_ref).await.map_err(err)?)
        }
        "library_remove" => {
            let a: ItemIdArg = dec(op, args)?;
            enc(&backend.library_remove(&a.item_id).await.map_err(err)?)
        }
        "fix_match" => {
            let a: FixMatchArgs = dec(op, args)?;
            enc(&backend
                .fix_match(&a.item_id, &a.target_ref)
                .await
                .map_err(err)?)
        }
        "units" => enc(&backend.units().await.map_err(err)?),
        other => Err(serde_json::Value::String(format!(
            "backend has no operation '{other}'"
        ))),
    }
}
