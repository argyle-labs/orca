//! Identity resolution — the canonical external-id taxonomy that makes
//! [`crate::merge_units`] actually converge.
//!
//! `merge_units` folds partial units by shared [`ExternalId`]s. That only works
//! if two backends reporting the *same* work emit the *same* `(source, id)`.
//! In practice they don't: one server says `imdb`/`tt0133093`, another says
//! `IMDB`/`0133093`; one reports an ISBN-13 with hyphens, another the bare
//! digits; Plex/Jellyfin/ABS expose ids as URI guids (`imdb://tt…`,
//! `plex://movie/…`, `com.plexapp.agents.imdb://tt…?lang=en`). Without a
//! normalization seam, units fragment onto the `(type, title, year)` fallback.
//!
//! This module defines:
//! * [`IdSource`] — the canonical namespace enum, its aliases, and which media
//!   types each is valid for.
//! * [`normalize_id`] / [`canonical_external_id`] — turn a raw `(source, id)`
//!   pair a backend scraped from metadata into a canonical, matchable
//!   [`ExternalId`].
//! * [`parse_guid`] — the resolution helper backends call on the guid strings
//!   Plex/Jellyfin/ABS hand out.
//! * [`MediaIdentity::primary_external_id`] — pick the best id by per-type
//!   source priority (for display and as a stable unit key).

use crate::{ExternalId, MediaIdentity, MediaType};
use thiserror::Error;

/// A canonical external-id namespace. `ExternalId::source` stays a free-form
/// `String` on the wire (plugins may carry sources we don't model yet), but any
/// source we *do* know is canonicalized through this enum so equal works match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdSource {
    /// The Movie Database — movies + tv.
    Tmdb,
    /// IMDb — movies + tv. Ids carry the `tt` prefix.
    Imdb,
    /// TheTVDB — tv (and some movies).
    Tvdb,
    /// MusicBrainz release/release-group MBID — music.
    MusicBrainz,
    /// International Standard Book Number — ebooks + audiobooks. Normalized to
    /// ISBN-13 digits with no separators.
    Isbn,
    /// Amazon Standard Identification Number (Audible/Kindle) — audiobooks + ebooks.
    Asin,
    /// Goodreads book id — ebooks + audiobooks.
    Goodreads,
    /// Hardcover book id — ebooks + audiobooks.
    Hardcover,
    /// Open Library id (`OL…M/W`) — ebooks.
    OpenLibrary,
    /// ComicVine issue/volume id — comics.
    ComicVine,
    /// Podcast Index feed id — podcasts.
    PodcastIndex,
}

impl IdSource {
    /// The one canonical wire string for this source. Aliases collapse to it.
    pub fn canonical(self) -> &'static str {
        match self {
            IdSource::Tmdb => "tmdb",
            IdSource::Imdb => "imdb",
            IdSource::Tvdb => "tvdb",
            IdSource::MusicBrainz => "musicbrainz",
            IdSource::Isbn => "isbn",
            IdSource::Asin => "asin",
            IdSource::Goodreads => "goodreads",
            IdSource::Hardcover => "hardcover",
            IdSource::OpenLibrary => "openlibrary",
            IdSource::ComicVine => "comicvine",
            IdSource::PodcastIndex => "podcastindex",
        }
    }

    /// Parse a raw source string (case-insensitive, alias-aware) into a known
    /// namespace. Returns `None` for sources we don't model — callers keep the
    /// raw string as a passthrough key rather than dropping the id.
    pub fn parse(raw: &str) -> Option<IdSource> {
        // Lowercase + strip a trailing `id`/`_id` and any `://` scheme tail so
        // `TMDB`, `tmdbId`, and `tmdb://` all land on the same namespace.
        let s = raw.trim().to_ascii_lowercase();
        let s = s.split_once("://").map_or(s.as_str(), |(head, _)| head);
        let s = s
            .strip_suffix("_id")
            .or_else(|| s.strip_suffix("id"))
            .unwrap_or(s)
            .trim_matches(['_', '-', ' ']);
        match s {
            "tmdb" | "themoviedb" | "moviedb" => Some(IdSource::Tmdb),
            "imdb" => Some(IdSource::Imdb),
            "tvdb" | "thetvdb" => Some(IdSource::Tvdb),
            "musicbrainz" | "mbid" | "mb" | "musicbrainz_album" | "musicbrainzalbum" => {
                Some(IdSource::MusicBrainz)
            }
            "isbn" | "isbn13" | "isbn10" => Some(IdSource::Isbn),
            "asin" | "audible" | "audible_asin" | "kindle" | "amazon" => Some(IdSource::Asin),
            "goodreads" | "gr" => Some(IdSource::Goodreads),
            "hardcover" => Some(IdSource::Hardcover),
            "openlibrary" | "olid" | "ol" => Some(IdSource::OpenLibrary),
            "comicvine" | "cv" | "cvid" => Some(IdSource::ComicVine),
            "podcastindex" | "podcast_index" | "pi" => Some(IdSource::PodcastIndex),
            _ => None,
        }
    }

    /// Whether this namespace is meaningful for `ty`. Used to reject a scraped id
    /// attached to the wrong type (an ISBN on a movie is a bug, not a match key).
    pub fn valid_for(self, ty: MediaType) -> bool {
        use MediaType::*;
        match self {
            IdSource::Tmdb => matches!(ty, Movies | Tv),
            IdSource::Imdb => matches!(ty, Movies | Tv),
            IdSource::Tvdb => matches!(ty, Tv | Movies),
            IdSource::MusicBrainz => matches!(ty, Music),
            IdSource::Isbn | IdSource::Goodreads | IdSource::Hardcover => {
                matches!(ty, Ebooks | Audiobooks)
            }
            // Audible ASINs are audiobooks; Kindle ASINs are ebooks.
            IdSource::Asin => matches!(ty, Audiobooks | Ebooks),
            IdSource::OpenLibrary => matches!(ty, Ebooks | Audiobooks),
            IdSource::ComicVine => matches!(ty, Comics),
            IdSource::PodcastIndex => matches!(ty, Podcasts),
        }
    }

    /// Merge-priority for this source within `ty` — lower is more authoritative,
    /// so [`MediaIdentity::primary_external_id`] and the matrix pick a stable
    /// canonical key. Unlisted sources sort last.
    pub fn priority_for(self, ty: MediaType) -> u8 {
        use MediaType::*;
        match (ty, self) {
            (Movies, IdSource::Tmdb) => 0,
            (Movies, IdSource::Imdb) => 1,
            (Tv, IdSource::Tvdb) => 0,
            (Tv, IdSource::Tmdb) => 1,
            (Tv, IdSource::Imdb) => 2,
            (Music, IdSource::MusicBrainz) => 0,
            (Audiobooks, IdSource::Asin) => 0,
            (Audiobooks, IdSource::Isbn) => 1,
            (Ebooks, IdSource::Isbn) => 0,
            (Ebooks, IdSource::Asin) => 1,
            (Comics, IdSource::ComicVine) => 0,
            (Podcasts, IdSource::PodcastIndex) => 0,
            _ => u8::MAX,
        }
    }
}

/// Normalize an id *value* for a known source so equal works produce equal
/// strings. Unknown sources are lowercased/trimmed as a best effort.
pub fn normalize_id(source: Option<IdSource>, raw: &str) -> String {
    let raw = raw.trim();
    match source {
        Some(IdSource::Imdb) => {
            // Bare digits or `tt`-prefixed → canonical `tt` + digits.
            let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                raw.to_ascii_lowercase()
            } else {
                format!("tt{digits}")
            }
        }
        Some(IdSource::Isbn) => normalize_isbn(raw),
        // ASIN / OpenLibrary are case-sensitive alphanumerics — uppercase ASIN,
        // keep OpenLibrary as-is but stripped.
        Some(IdSource::Asin) => raw.to_ascii_uppercase(),
        Some(IdSource::OpenLibrary) => raw.trim().to_string(),
        Some(IdSource::ComicVine) if let Ok(id) = comicvine_volume_id(raw) => id,
        // Numeric/opaque ids: strip surrounding noise, drop a trailing query.
        Some(_) => raw
            .split(['?', '#'])
            .next()
            .unwrap_or(raw)
            .trim_matches('/')
            .to_ascii_lowercase(),
        None => raw.to_ascii_lowercase(),
    }
}

/// Canonicalize a raw `(source, id)` pair into a matchable [`ExternalId`], or
/// `None` if the id is empty. Unknown sources pass through with a normalized id
/// so they can still act as a (weaker) match key. When `ty` is given, an id
/// attached to a source invalid for that type is rejected (`None`).
pub fn canonical_external_id(source: &str, id: &str, ty: Option<MediaType>) -> Option<ExternalId> {
    if id.trim().is_empty() {
        return None;
    }
    let parsed = IdSource::parse(source);
    if let (Some(src), Some(ty)) = (parsed, ty)
        && !src.valid_for(ty)
    {
        return None;
    }
    let canonical_source = parsed.map_or_else(
        || source.trim().to_ascii_lowercase(),
        |s| s.canonical().to_string(),
    );
    let norm = normalize_id(parsed, id);
    if norm.is_empty() {
        return None;
    }
    Some(ExternalId {
        source: canonical_source,
        id: norm,
    })
}

/// The resolution helper backends call on the guid strings media servers hand
/// out. Handles the common shapes:
/// * `imdb://tt0133093`, `tmdb://603`, `tvdb://12345`
/// * `com.plexapp.agents.imdb://tt0133093?lang=en` (legacy Plex agent guids)
/// * bare `tt0133093` (assumed IMDb)
/// * ComicVine volume URLs (`https://comicvine.gamespot.com/x/4050-17993/`)
///
/// Returns `None` for guids we can't attribute (e.g. `plex://movie/5d…`, a
/// server-local surrogate that is not a cross-backend key).
pub fn parse_guid(guid: &str) -> Option<ExternalId> {
    let g = guid.trim();
    if g.is_empty() {
        return None;
    }
    // Bare `tt…` with no scheme → IMDb.
    if g.starts_with("tt") && g[2..].chars().all(|c| c.is_ascii_digit()) && g.len() > 2 {
        return canonical_external_id("imdb", g, None);
    }
    if is_comicvine_url(g) {
        let id = comicvine_volume_id(g).ok()?;
        return canonical_external_id("comicvine", &id, None);
    }
    let (scheme, rest) = g.split_once("://")?;
    // Legacy Plex agent guids: `com.plexapp.agents.<src>`.
    let scheme = scheme
        .rsplit_once('.')
        .map_or(scheme, |(_, tail)| tail)
        .trim();
    // Strip a query/fragment and any path tail after the id.
    let id = rest.split(['?', '#']).next().unwrap_or(rest);
    let id = id.split('/').next_back().unwrap_or(id);
    // Only surface guids that map to a known cross-backend namespace; a
    // server-local scheme (plex/jellyfin/local) is not a merge key.
    IdSource::parse(scheme)?;
    canonical_external_id(scheme, id, None)
}

const COMICVINE_VOLUME: &str = "4050-";
const COMICVINE_HOST: &str = "comicvine.gamespot.com";

/// Why ComicVine input could not be read as volume ids.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ComicVineIdError {
    #[error("'{0}' is not a ComicVine volume id")]
    NotAnId(String),
    #[error("'{0}' is not a ComicVine volume URL; give a volume (4050-) URL or id")]
    NotAVolumeUrl(String),
    #[error(
        "'{token}' is ambiguous: '{tail}' could be part of the URL or a separate id; \
         pass '{tail}' as its own id or before the URL"
    )]
    Ambiguous { token: String, tail: String },
    #[error("give at least one ComicVine id")]
    Empty,
}

/// Parse one ComicVine volume reference into canonical `4050-N` form: a bare id
/// (`12345`), the volume form (`4050-12345`), or a volume URL with or without a
/// scheme (`https://comicvine.gamespot.com/invincible/4050-17993/`).
pub fn comicvine_volume_id(raw: &str) -> Result<String, ComicVineIdError> {
    let token = raw.trim();
    let id = if is_comicvine_url(token) {
        let path = token.split(['?', '#']).next().unwrap_or(token);
        path.split('/')
            .find(|seg| seg.starts_with(COMICVINE_VOLUME))
            .ok_or_else(|| ComicVineIdError::NotAVolumeUrl(token.to_string()))?
    } else {
        token
    };
    let digits = id.strip_prefix(COMICVINE_VOLUME).unwrap_or(id).trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ComicVineIdError::NotAnId(token.to_string()));
    }
    let digits = match digits.trim_start_matches('0') {
        "" => "0",
        d => d,
    };
    Ok(format!("{COMICVINE_VOLUME}{digits}"))
}

/// Parse a list of ComicVine volume references, each entry possibly holding
/// several comma-separated ids/URLs, into deduplicated canonical `4050-N` ids in
/// first-seen order.
pub fn comicvine_volume_ids<S: AsRef<str>>(inputs: &[S]) -> Result<Vec<String>, ComicVineIdError> {
    let mut out: Vec<String> = Vec::new();
    for input in inputs {
        for token in split_comicvine_list(input.as_ref())? {
            if token.trim().is_empty() {
                continue;
            }
            let id = comicvine_volume_id(token)?;
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    if out.is_empty() {
        return Err(ComicVineIdError::Empty);
    }
    Ok(out)
}

fn is_comicvine_url(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let host = rest.strip_prefix("www.").unwrap_or(rest);
    host == COMICVINE_HOST || host.starts_with(&format!("{COMICVINE_HOST}/"))
}

// Commas are legal in a URL query or fragment, so once a URL reaches `?`/`#` only
// another URL ends it; a trailing id-shaped item there is ambiguous and rejected.
fn split_comicvine_list(s: &str) -> Result<Vec<&str>, ComicVineIdError> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, _) in s.match_indices(',') {
        let cur = s[start..i].trim_start();
        let rest = s[i + 1..].trim_start();
        if is_comicvine_url(cur) && cur.contains(['?', '#']) && !is_comicvine_url(rest) {
            continue;
        }
        out.push(&s[start..i]);
        start = i + 1;
    }
    out.push(&s[start..]);
    for tok in &out {
        let tok = tok.trim();
        let Some(q) = tok.find(['?', '#']) else {
            continue;
        };
        if !is_comicvine_url(tok) {
            continue;
        }
        if let Some((_, tail)) = tok[q..].rsplit_once(',') {
            let tail = tail.trim();
            let digits = tail.strip_prefix(COMICVINE_VOLUME).unwrap_or(tail);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ComicVineIdError::Ambiguous {
                    token: tok.to_string(),
                    tail: tail.to_string(),
                });
            }
        }
    }
    Ok(out)
}

/// Normalize an ISBN to bare ISBN-13 digits (converting a valid ISBN-10 by
/// re-prefixing `978` + recomputing the check digit). Falls back to the stripped
/// input when it isn't a recognizable ISBN-10/13.
fn normalize_isbn(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'x' || *c == 'X')
        .collect();
    match cleaned.len() {
        13 if cleaned.chars().all(|c| c.is_ascii_digit()) => cleaned,
        10 => isbn10_to_13(&cleaned).unwrap_or(cleaned),
        _ => cleaned.to_ascii_uppercase(),
    }
}

/// Convert a 10-char ISBN-10 (last char may be `X`) to ISBN-13 digits.
fn isbn10_to_13(isbn10: &str) -> Option<String> {
    if isbn10.len() != 10 {
        return None;
    }
    // First 9 must be digits (the 10th is the ISBN-10 check we discard).
    let core: String = isbn10.chars().take(9).collect();
    if !core.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let base = format!("978{core}");
    let sum: u32 = base
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let d = c.to_digit(10).unwrap_or(0);
            if i % 2 == 0 { d } else { d * 3 }
        })
        .sum();
    let check = (10 - (sum % 10)) % 10;
    Some(format!("{base}{check}"))
}

impl MediaIdentity {
    /// Attach a raw `(source, id)` pair, canonicalizing and de-duplicating it.
    /// The builder backends use while assembling their partial unit view — it
    /// keeps every id matchable without each backend re-implementing
    /// normalization. Ids invalid for `media_type` are silently skipped.
    pub fn with_external_id(
        mut self,
        source: &str,
        id: &str,
        media_type: MediaType,
    ) -> MediaIdentity {
        if let Some(ext) = canonical_external_id(source, id, Some(media_type))
            && !self.external_ids.contains(&ext)
        {
            self.external_ids.push(ext);
        }
        self
    }

    /// The most authoritative external id for `media_type`, by source priority
    /// (see [`IdSource::priority_for`]). A stable display/merge key when several
    /// ids are present. `None` when the identity carries no external ids.
    pub fn primary_external_id(&self, media_type: MediaType) -> Option<&ExternalId> {
        self.external_ids.iter().min_by_key(|e| {
            IdSource::parse(&e.source).map_or(u8::MAX, |s| s.priority_for(media_type))
        })
    }
}

/// Canonicalize an [`ExternalId`] in place-by-value: map the source to its
/// canonical namespace and normalize the id. Used by [`crate::merge_units`] so
/// its match keys are computed on canonical form regardless of what each backend
/// emitted. Unknown sources are lowercased and pass through. Public so the
/// `media.unit.detail` tool can key a user-supplied `--id source:id` on the same
/// canonical form the merge indexed on.
pub fn canonicalize(ext: &ExternalId) -> ExternalId {
    canonical_external_id(&ext.source, &ext.id, None).unwrap_or_else(|| ExternalId {
        source: ext.source.trim().to_ascii_lowercase(),
        id: ext.id.trim().to_ascii_lowercase(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_aliases_collapse() {
        assert_eq!(IdSource::parse("TMDB"), Some(IdSource::Tmdb));
        assert_eq!(IdSource::parse("tmdbId"), Some(IdSource::Tmdb));
        assert_eq!(IdSource::parse("audible_asin"), Some(IdSource::Asin));
        assert_eq!(IdSource::parse("mbid"), Some(IdSource::MusicBrainz));
        assert_eq!(IdSource::parse("tmdb://"), Some(IdSource::Tmdb));
        assert_eq!(IdSource::parse("whatever"), None);
    }

    #[test]
    fn imdb_id_normalizes_to_tt_prefix() {
        let a = canonical_external_id("IMDB", "0133093", None).unwrap();
        let b = canonical_external_id("imdb", "tt0133093", None).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.source, "imdb");
        assert_eq!(a.id, "tt0133093");
    }

    #[test]
    fn isbn10_and_13_and_hyphenated_agree() {
        // 0-306-40615-2 (ISBN-10) ⇔ 978-0-306-40615-7 (ISBN-13).
        let a = canonical_external_id("isbn", "0-306-40615-2", None).unwrap();
        let b = canonical_external_id("ISBN13", "9780306406157", None).unwrap();
        assert_eq!(a.id, "9780306406157");
        assert_eq!(a, b);
    }

    #[test]
    fn guid_shapes_resolve() {
        assert_eq!(
            parse_guid("imdb://tt0133093"),
            canonical_external_id("imdb", "tt0133093", None)
        );
        assert_eq!(
            parse_guid("com.plexapp.agents.imdb://tt0133093?lang=en"),
            canonical_external_id("imdb", "tt0133093", None)
        );
        assert_eq!(
            parse_guid("tt0133093"),
            canonical_external_id("imdb", "tt0133093", None)
        );
        // Server-local surrogate — not a cross-backend key.
        assert_eq!(parse_guid("plex://movie/5d776b59"), None);
    }

    #[test]
    fn wrong_type_id_is_rejected() {
        // An ISBN on a movie is a mis-scrape, not a merge key.
        assert_eq!(
            canonical_external_id("isbn", "9780306406157", Some(MediaType::Movies)),
            None
        );
    }

    #[test]
    fn primary_id_picks_by_priority() {
        let id = MediaIdentity {
            title: "The Matrix".into(),
            year: Some(1999),
            external_ids: vec![],
            series: None,
        }
        .with_external_id("imdb", "tt0133093", MediaType::Movies)
        .with_external_id("tmdb", "603", MediaType::Movies);
        // tmdb outranks imdb for movies.
        assert_eq!(
            id.primary_external_id(MediaType::Movies).unwrap().source,
            "tmdb"
        );
    }

    fn cv(inputs: &[&str]) -> Result<Vec<String>, ComicVineIdError> {
        comicvine_volume_ids(inputs)
    }

    #[test]
    fn comicvine_ids_accept_volume_form_and_dedupe() {
        assert_eq!(
            cv(&["4050-12345, 678", "12345"]).unwrap(),
            vec!["4050-12345", "4050-678"]
        );
        assert!(cv(&["Saga"]).is_err());
        assert_eq!(cv(&[" "]), Err(ComicVineIdError::Empty));
    }

    #[test]
    fn comicvine_ids_accept_volume_urls() {
        let ids = cv(&[
            "https://comicvine.gamespot.com/invincible/4050-17993/",
            "http://comicvine.gamespot.com/saga/4050-48771",
            "https://comicvine.gamespot.com/x/4050-0042/?utm_source=a&b=4000-1",
        ])
        .unwrap();
        assert_eq!(ids, vec!["4050-17993", "4050-48771", "4050-42"]);
    }

    #[test]
    fn comicvine_ids_keep_commas_inside_a_url_query() {
        let ids = cv(&[
            "678,https://comicvine.gamespot.com/x/4050-17993/?a=1,b,https://comicvine.gamespot.com/y/4050-48771/#c,d",
            "9",
        ])
        .unwrap();
        assert_eq!(ids, vec!["4050-678", "4050-17993", "4050-48771", "4050-9"]);
        assert_eq!(cv(&["1,2,3"]).unwrap(), vec!["4050-1", "4050-2", "4050-3"]);
        assert_eq!(
            cv(&["https://comicvine.gamespot.com/x/4050-17993/,5"]).unwrap(),
            vec!["4050-17993", "4050-5"]
        );
        assert_eq!(
            cv(&["https://comicvine.gamespot.com/x/4050-1/?a=1,b,https://comicvine.gamespot.com/y/4050-9/"])
                .unwrap(),
            vec!["4050-1", "4050-9"]
        );
    }

    #[test]
    fn comicvine_ids_reject_an_id_swallowed_by_a_url_query() {
        for input in [
            "https://comicvine.gamespot.com/x/4050-1/?ref=a,2",
            "https://comicvine.gamespot.com/x/4050-1/#frag,4050-5",
        ] {
            let err = cv(&[input]).unwrap_err();
            assert!(matches!(err, ComicVineIdError::Ambiguous { .. }), "{err}");
            assert!(err.to_string().contains(input), "{err}");
        }
    }

    #[test]
    fn comicvine_ids_reject_non_volume_urls() {
        let err = cv(&["https://comicvine.gamespot.com/invincible-1/4000-33017/"]).unwrap_err();
        assert!(
            err.to_string().contains("volume (4050-) URL or id"),
            "{err}"
        );
        assert!(cv(&["https://comicvine.gamespot.com/mark-grayson/4005-11/"]).is_err());
    }

    #[test]
    fn comicvine_ids_accept_any_case_and_schemeless_urls() {
        let ids = cv(&[
            "HTTPS://comicvine.gamespot.com/x/4050-1/",
            "comicvine.gamespot.com/x/4050-2/",
            "WWW.ComicVine.gamespot.com/x/4050-3",
        ])
        .unwrap();
        assert_eq!(ids, vec!["4050-1", "4050-2", "4050-3"]);
        assert_eq!(
            cv(&["comicvine.gamespot.com/x/4050-4/?a=1,b,www.comicvine.gamespot.com/y/4050-5/"])
                .unwrap(),
            vec!["4050-4", "4050-5"]
        );
        assert!(cv(&["comicvine.gamespot.com/x/4050-1/?ref=a,2"]).is_err());
        let err = cv(&["comicvine.gamespot.com/x/4000-5/"]).unwrap_err();
        assert!(
            err.to_string().contains("volume (4050-) URL or id"),
            "{err}"
        );
    }

    #[test]
    fn comicvine_ids_reject_a_bare_volume_prefix() {
        let err = cv(&["4050-"]).unwrap_err();
        assert!(err.to_string().contains("'4050-'"), "{err}");
        assert!(cv(&["4050- ", "1"]).is_err());
    }

    #[test]
    fn comicvine_ids_dedupe_on_canonical_form() {
        assert_eq!(
            cv(&["012345, 12345", "4050-0012345"]).unwrap(),
            vec!["4050-12345"]
        );
        assert_eq!(cv(&["000"]).unwrap(), vec!["4050-0"]);
    }

    #[test]
    fn comicvine_ids_fold_into_external_ids() {
        let want = canonical_external_id("comicvine", "4050-17993", None).unwrap();
        assert_eq!(want.id, "4050-17993");
        assert_eq!(canonical_external_id("cv", "017993", None).unwrap(), want);
        assert_eq!(
            parse_guid("https://comicvine.gamespot.com/invincible/4050-17993/?a=1"),
            Some(want.clone())
        );
        assert_eq!(parse_guid("comicvine://4050-17993"), Some(want));
        assert_eq!(
            canonical_external_id("comicvine", "4000-33017", None)
                .unwrap()
                .id,
            "4000-33017"
        );
        assert_eq!(parse_guid("comicvine.gamespot.com/x/4000-5/"), None);
        assert_eq!(parse_guid("https://example.com/x/4050-1/"), None);
        assert_eq!(
            parse_guid("https://comicvine.gamespot.com.evil.test/x/4050-1/"),
            None
        );
        assert!(cv(&["https://example.com/x/4050-1/"]).is_err());
    }
}
