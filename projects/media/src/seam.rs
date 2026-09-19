//! The two uniform media adapter seams — the KEYSTONE of epic #494 (#495).
//!
//! Every media plugin conforms to exactly one of two canonical seams, with
//! IDENTICAL verb sets and IDENTICAL request/response shapes across all
//! backends. This is what lets the generic orca `media`-crate brain drive them
//! all: it dispatches over the trait, never over N backend APIs.
//!
//! - [`Requester`] — WHAT to get. LazyLibrarian, Mylar3, Sonarr, Radarr(+4k),
//!   Lidarr. Uniform verbs: add/monitor, want+search, status, blocklist,
//!   import-trigger.
//! - [`LibraryServer`] — serve / organize. Calibre, Komga, Audiobookshelf, Plex,
//!   Jellyfin, Navidrome. Uniform verbs: scan/refresh(scope), collection/series/
//!   readlist management, metadata read/set, library-state read.
//!
//! **Hard rule (conformance).** All backend-specific quirks are handled INSIDE
//! the adapter and MUST NOT appear in the seam:
//! - LazyLibrarian records Wanted/Snatched on a SIBLING edition row, not the
//!   canonical bookid → the adapter aggregates across editions before returning
//!   a [`RequestStatus`]; [`RequestState`] is edition-normalized so the sibling
//!   trap never leaks. (#497)
//! - Mylar CDH import race, Radarr/Radarr-4k category split, Calibre
//!   `embed_metadata`, Plex section scoping → all hidden behind the uniform
//!   verbs.
//!
//! Any adapter that needs a bespoke verb is a signal the seam is wrong, not a
//! license to special-case. The conformance checklist every adapter PR must
//! satisfy lives at the bottom of this file (`CONFORMANCE`).

use crate::{MediaError, MediaIdentity, MediaMutation, MediaType};
use derive::orca_async;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

// ── Shared request/response types (identical across every adapter) ────────────

/// Quality / format policy for a request. Generic across media types: the
/// tokens mean resolution for video (`2160p`), container/codec for audio
/// (`m4b`, `flac`), or format for books/comics (`epub`, `cbz`). The adapter
/// maps these onto its backend's native quality profile — orca never learns a
/// backend's profile ids.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QualityPolicy {
    /// Preferred quality/format tokens, best first. Empty = backend default.
    #[serde(default)]
    pub preferred: Vec<String>,
    /// Hard rejects — a release matching any of these is never grabbed
    /// (`["cam", "upscaled"]`). Encodes the "junk-edition rejection" of #499.
    #[serde(default)]
    pub reject: Vec<String>,
    /// Keep upgrading until the top preferred token is met, then stop.
    #[serde(default)]
    pub upgrade_until_met: bool,
}

/// A specific release/candidate a requester found or acted on. The unit of
/// blocklisting (#501) and the "what's acting" pointer inside a status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseRef {
    /// Requester-local release id (nzb id, torrent guid, download id).
    pub id: String,
    /// Human release title (`Book.Title.2020.RETAIL.EPUB-GRP`), when known.
    #[serde(default)]
    pub title: Option<String>,
    /// Indexer/source the release came from (`nzbgeek`, `mam`), when known.
    #[serde(default)]
    pub source: Option<String>,
}

/// Edition/state-normalized request state. This enum is the seam's defense
/// against LazyLibrarian's sibling-edition trap (#497): the adapter aggregates
/// every edition/row it holds for one identity down to a SINGLE state here, so a
/// caller never sees a false `Skipped` because status landed on a sibling row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    /// Monitored, not yet searched.
    Wanted,
    /// A search is in flight.
    Searching,
    /// A release was grabbed and handed to a download client.
    Snatched,
    /// Bytes are transferring.
    Downloading,
    /// Completed and imported into the library.
    Imported,
    /// The acquisition failed (dead torrent, aborted usenet, wrong format).
    Failed,
    /// Deliberately not wanted / ignored by the backend.
    Skipped,
    /// State could not be determined.
    Unknown,
}

impl RequestState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestState::Wanted => "wanted",
            RequestState::Searching => "searching",
            RequestState::Snatched => "snatched",
            RequestState::Downloading => "downloading",
            RequestState::Imported => "imported",
            RequestState::Failed => "failed",
            RequestState::Skipped => "skipped",
            RequestState::Unknown => "unknown",
        }
    }
}

/// Normalized status of one request, aggregated across ALL editions/rows the
/// backend holds for the identity. This is the canonical read the generic
/// completeness reconcile (#496) and verified-status read (#497) consume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestStatus {
    pub identity: MediaIdentity,
    /// Edition-normalized state (aggregated across every edition/row).
    pub state: RequestState,
    /// The release currently acting for this identity, if any.
    #[serde(default)]
    pub active_release: Option<ReleaseRef>,
    /// Backend-native detail for display/debug (never parsed by the brain).
    #[serde(default)]
    pub detail: Option<String>,
}

/// Args for [`Requester::add`] — add/monitor an item by identity under a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AddRequest {
    pub identity: MediaIdentity,
    #[serde(default)]
    pub quality: QualityPolicy,
    /// Kick a search immediately vs monitor-only.
    #[serde(default)]
    pub search_now: bool,
}

/// One search candidate. The rate-limit-aware search driver (#500) and strict
/// matcher (#499) rank/filter over these before the brain decides to grab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub release: ReleaseRef,
    /// The identity the adapter believes this hit matches, when resolvable.
    #[serde(default)]
    pub identity: Option<MediaIdentity>,
    /// Quality/format token of the release (`1080p`, `epub`), when known.
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Torrent seeders, when applicable.
    #[serde(default)]
    pub seeders: Option<u32>,
    /// Usenet age in seconds, when applicable.
    #[serde(default)]
    pub age_secs: Option<u64>,
}

// ── LibraryServer shared types ────────────────────────────────────────────────

/// The target of a scan/refresh or library-state read. Uniform across every
/// library backend — the adapter maps it onto native section/path semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Scope {
    /// The whole library for this media type on this backend.
    All,
    /// A specific backend library/section id (`plex` section, calibre virtual lib).
    Library(String),
    /// A single item by identity.
    Item(MediaIdentity),
    /// A filesystem path — the targeted scan a post-import fulfillment fires (#502).
    Path(String),
}

/// A grouping the server organizes: a collection, a series, or an ordered
/// readlist. One uniform shape; `kind` distinguishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollectionKind {
    /// An arbitrary curated set (Plex collection, Komga collection).
    Collection,
    /// A series/sequence (ordered by identity's `series.sequence`).
    Series,
    /// An explicitly ordered reading order (Komga readlist, Calibre).
    Readlist,
}

/// A collection/series/readlist with its ordered members (member order is the
/// vec order). The uniform object `collection_get`/`collection_set` speak.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Collection {
    /// Backend-local id. Empty on create; the backend assigns and echoes it.
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub kind: CollectionKind,
    /// Ordered members, by identity (never by backend-local item id — order and
    /// membership are identity-keyed so they survive a re-scan / re-match).
    #[serde(default)]
    pub items: Vec<MediaIdentity>,
}

/// Generic item metadata as key/value. The adapter maps keys onto native fields
/// (`title`, `sort_title`, `series`, `series_index`, `summary`, …); orca never
/// learns a backend's metadata schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Metadata {
    pub identity: MediaIdentity,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// A lightweight library item — enough for completeness reconcile (#496) to
/// compute owned-vs-wanted without pulling full [`crate::MediaUnit`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LibraryEntry {
    pub identity: MediaIdentity,
    /// Backend-local item id, for targeted follow-up ops.
    #[serde(default)]
    pub id: Option<String>,
}

/// The library-state read for a scope: what the server currently holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LibraryState {
    pub item_count: u64,
    #[serde(default)]
    pub items: Vec<LibraryEntry>,
}

// ── Seam 1: Requester (WHAT to get) ──────────────────────────────────────────

/// A requester backend for ONE media type. Every requester (LazyLibrarian,
/// Mylar3, Sonarr, Radarr, Lidarr) implements this IDENTICAL verb set. Backend
/// quirks are hidden inside the adapter; nothing backend-specific appears here.
#[orca_async]
pub trait Requester: Send + Sync {
    /// App identity (`lazylibrarian`). Registry key is `(name, media_type)`.
    fn name(&self) -> &str;
    /// The media type this registration requests.
    fn media_type(&self) -> MediaType;

    /// Add/monitor an item by identity under a quality/format policy. Returns
    /// the normalized status after the add (typically `Wanted`/`Searching`).
    async fn add(&self, req: &AddRequest) -> Result<RequestStatus, MediaError>;

    /// Want + search: mark a known identity wanted and run a search now,
    /// returning candidate hits. (The brain, not the adapter, decides to grab.)
    async fn search(&self, identity: &MediaIdentity) -> Result<Vec<SearchHit>, MediaError>;

    /// Edition/state-normalized status for an identity, aggregated across ALL
    /// editions/rows the backend holds (this is where #497 lives).
    async fn status(&self, identity: &MediaIdentity) -> Result<RequestStatus, MediaError>;

    /// Blocklist a bad release so it is never re-grabbed, then re-search.
    /// Breaks retry-storms (#501): duplicate-NZB loops, dead torrents.
    async fn blocklist(
        &self,
        identity: &MediaIdentity,
        release: &ReleaseRef,
    ) -> Result<RequestStatus, MediaError>;

    /// Trigger import/post-process of any completed-but-unimported downloads
    /// for this identity (Mylar forceProcess, LL library-scan, arr manual import).
    async fn import_trigger(&self, identity: &MediaIdentity) -> Result<MediaMutation, MediaError>;
}

// ── Seam 2: LibraryServer (serve / organize) ─────────────────────────────────

/// A library-server backend for ONE media type. Every server (Calibre, Komga,
/// Audiobookshelf, Plex, Jellyfin, Navidrome) implements this IDENTICAL verb
/// set. Backend quirks (Plex section scoping, ABS rotating JWT, Calibre embed)
/// are hidden inside the adapter.
#[orca_async]
pub trait LibraryServer: Send + Sync {
    /// App identity (`calibre`). Registry key is `(name, media_type)`.
    fn name(&self) -> &str;
    /// The media type this registration serves.
    fn media_type(&self) -> MediaType;

    /// Scan/refresh the given scope (whole library, section, item, or a path).
    async fn scan(&self, scope: &Scope) -> Result<MediaMutation, MediaError>;

    /// Read a collection/series/readlist by backend-local id.
    async fn collection_get(&self, id: &str) -> Result<Collection, MediaError>;

    /// Create or update a collection/series/readlist (membership + order).
    /// Empty `Collection::id` = create; the returned mutation carries the new id.
    async fn collection_set(&self, collection: &Collection) -> Result<MediaMutation, MediaError>;

    /// Read metadata for an item by identity.
    async fn metadata_get(&self, identity: &MediaIdentity) -> Result<Metadata, MediaError>;

    /// Set metadata for an item.
    async fn metadata_set(&self, metadata: &Metadata) -> Result<MediaMutation, MediaError>;

    /// Read library state (count + entries) for a scope.
    async fn library_state(&self, scope: &Scope) -> Result<LibraryState, MediaError>;
}

// ── Process-global registries (one per seam) ─────────────────────────────────

static REQUESTERS: LazyLock<RwLock<Vec<Arc<dyn Requester>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));
static SERVERS: LazyLock<RwLock<Vec<Arc<dyn LibraryServer>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a requester. Re-registering the same `(name, media_type)` replaces
/// the entry so a plugin reload doesn't duplicate providers.
pub fn register_requester(r: Arc<dyn Requester>) {
    let mut g = REQUESTERS.write().expect("requester registry poisoned");
    let (name, ty) = (r.name().to_string(), r.media_type());
    if let Some(slot) = g
        .iter_mut()
        .find(|b| b.name() == name && b.media_type() == ty)
    {
        *slot = r;
    } else {
        g.push(r);
    }
}

/// Register a library server. Re-registering `(name, media_type)` replaces.
pub fn register_server(s: Arc<dyn LibraryServer>) {
    let mut g = SERVERS.write().expect("server registry poisoned");
    let (name, ty) = (s.name().to_string(), s.media_type());
    if let Some(slot) = g
        .iter_mut()
        .find(|b| b.name() == name && b.media_type() == ty)
    {
        *slot = s;
    } else {
        g.push(s);
    }
}

/// Every registered requester for a media type (`lazylibrarian` for ebooks).
pub fn requesters_for(media_type: MediaType) -> Vec<Arc<dyn Requester>> {
    REQUESTERS
        .read()
        .expect("requester registry poisoned")
        .iter()
        .filter(|b| b.media_type() == media_type)
        .cloned()
        .collect()
}

/// Every registered library server for a media type.
pub fn servers_for(media_type: MediaType) -> Vec<Arc<dyn LibraryServer>> {
    SERVERS
        .read()
        .expect("server registry poisoned")
        .iter()
        .filter(|b| b.media_type() == media_type)
        .cloned()
        .collect()
}

/// Look up one requester by `(name, media_type)`.
pub fn requester(name: &str, media_type: MediaType) -> Option<Arc<dyn Requester>> {
    REQUESTERS
        .read()
        .expect("requester registry poisoned")
        .iter()
        .find(|b| b.name() == name && b.media_type() == media_type)
        .cloned()
}

/// Look up one library server by `(name, media_type)`.
pub fn server(name: &str, media_type: MediaType) -> Option<Arc<dyn LibraryServer>> {
    SERVERS
        .read()
        .expect("server registry poisoned")
        .iter()
        .find(|b| b.name() == name && b.media_type() == media_type)
        .cloned()
}

/// Deregister every requester + server registered under `name` (all media
/// types) — the unload path a plugin's domain-registration needs. Returns the
/// total count removed across both registries.
pub fn deregister(name: &str) -> usize {
    let mut removed = 0;
    {
        let mut g = REQUESTERS.write().expect("requester registry poisoned");
        let before = g.len();
        g.retain(|b| b.name() != name);
        removed += before - g.len();
    }
    {
        let mut g = SERVERS.write().expect("server registry poisoned");
        let before = g.len();
        g.retain(|b| b.name() != name);
        removed += before - g.len();
    }
    removed
}

// ── Conformance checklist ─────────────────────────────────────────────────────

/// The checklist every adapter PR (#498, #504–#509, …) must satisfy. Kept in
/// source so it travels with the seam and can be linked from each adapter PR.
///
/// A requester adapter conforms iff:
/// 1. It implements [`Requester`] and NOTHING else — no bespoke public verb.
/// 2. `status` aggregates across EVERY edition/row for the identity and returns
///    one edition-normalized [`RequestState`] (the LL sibling-row trap, #497).
/// 3. `search` returns candidates only; it never auto-grabs — the brain decides.
/// 4. `blocklist` both records the bad release AND re-searches (anti-retry, #501).
/// 5. Identity is passed as [`MediaIdentity`] (external ids); the adapter maps to
///    its backend's native id internally. No backend id crosses the seam inbound.
/// 6. Quality/format intent is [`QualityPolicy`] tokens; the adapter maps to its
///    native quality profile. Backends never leak profile ids.
///
/// A library-server adapter conforms iff:
/// 1. It implements [`LibraryServer`] and nothing else.
/// 2. `scan(Scope::Path)` does a TARGETED scan (post-import tail, #502), not a
///    full-library rescan — delta over full-rescan (standing perf directive).
/// 3. Collections/series/readlists are identity-keyed (`Collection::items` are
///    [`MediaIdentity`]), so membership survives a re-match/re-scan.
/// 4. `metadata_*` speak the generic k/v [`Metadata`]; the adapter maps keys.
/// 5. Rotating-auth backends (ABS JWT) refresh internally; auth never leaks.
pub const CONFORMANCE: &str = "see media::seam::CONFORMANCE doc comment";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExternalId;

    fn id(title: &str, isbn: &str) -> MediaIdentity {
        MediaIdentity {
            title: title.into(),
            year: None,
            external_ids: vec![ExternalId {
                source: "isbn".into(),
                id: isbn.into(),
            }],
            series: None,
        }
    }

    // A minimal conforming requester, proving the trait is object-safe and
    // implementable with the uniform verb set only.
    struct FakeReq;
    #[orca_async]
    impl Requester for FakeReq {
        fn name(&self) -> &str {
            "fake"
        }
        fn media_type(&self) -> MediaType {
            MediaType::Ebooks
        }
        async fn add(&self, req: &AddRequest) -> Result<RequestStatus, MediaError> {
            Ok(RequestStatus {
                identity: req.identity.clone(),
                state: RequestState::Wanted,
                active_release: None,
                detail: None,
            })
        }
        async fn search(&self, _identity: &MediaIdentity) -> Result<Vec<SearchHit>, MediaError> {
            Ok(vec![])
        }
        async fn status(&self, identity: &MediaIdentity) -> Result<RequestStatus, MediaError> {
            Ok(RequestStatus {
                identity: identity.clone(),
                state: RequestState::Imported,
                active_release: None,
                detail: None,
            })
        }
        async fn blocklist(
            &self,
            identity: &MediaIdentity,
            _release: &ReleaseRef,
        ) -> Result<RequestStatus, MediaError> {
            Ok(RequestStatus {
                identity: identity.clone(),
                state: RequestState::Searching,
                active_release: None,
                detail: None,
            })
        }
        async fn import_trigger(
            &self,
            _identity: &MediaIdentity,
        ) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
    }

    struct FakeSrv;
    #[orca_async]
    impl LibraryServer for FakeSrv {
        fn name(&self) -> &str {
            "fake-srv"
        }
        fn media_type(&self) -> MediaType {
            MediaType::Ebooks
        }
        async fn scan(&self, _scope: &Scope) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
        async fn collection_get(&self, id: &str) -> Result<Collection, MediaError> {
            Ok(Collection {
                id: id.into(),
                name: "s".into(),
                kind: CollectionKind::Series,
                items: vec![],
            })
        }
        async fn collection_set(&self, _c: &Collection) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
        async fn metadata_get(&self, identity: &MediaIdentity) -> Result<Metadata, MediaError> {
            Ok(Metadata {
                identity: identity.clone(),
                fields: BTreeMap::new(),
            })
        }
        async fn metadata_set(&self, _m: &Metadata) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
        async fn library_state(&self, _scope: &Scope) -> Result<LibraryState, MediaError> {
            Ok(LibraryState {
                item_count: 0,
                items: vec![],
            })
        }
    }

    #[tokio::test]
    async fn registries_dedupe_and_lookup_by_identity() {
        register_requester(Arc::new(FakeReq));
        register_requester(Arc::new(FakeReq)); // same (name, type) → replace
        register_server(Arc::new(FakeSrv));

        assert_eq!(requesters_for(MediaType::Ebooks).len(), 1);
        assert_eq!(servers_for(MediaType::Ebooks).len(), 1);
        assert!(requester("fake", MediaType::Ebooks).is_some());
        assert!(server("fake-srv", MediaType::Ebooks).is_some());
        assert!(requester("fake", MediaType::Comics).is_none());

        // The uniform verbs are callable over the trait object.
        let r = requester("fake", MediaType::Ebooks).unwrap();
        let st = r.status(&id("Dune", "9780441172719")).await.unwrap();
        assert_eq!(st.state, RequestState::Imported);

        assert_eq!(deregister("fake"), 1);
        assert_eq!(deregister("fake-srv"), 1);
        assert_eq!(requesters_for(MediaType::Ebooks).len(), 0);
    }
}
