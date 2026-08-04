//! Shared pagination primitive for every `*.list` verb on the orca surface.
//!
//! The hard rule: `list` is thin + fast + paginated; `detail` carries the heavy
//! per-record data. Every list verb embeds [`PageParams`] in its args and returns
//! a [`Page<T>`] of thin rows — never an unbounded `Vec`. Clients page by passing
//! the previous page's `next_cursor` back in.
//!
//! The cursor is an **opaque token**: clients must treat it as a blob and only
//! ever echo it back. Today it encodes a simple offset (uniform for the many
//! in-memory computed lists); DB-backed hot paths may switch to a keyset cursor
//! over the record's uuidv7 id later WITHOUT changing this type — that is the
//! whole point of keeping it opaque.
//!
//! Types live here (dep-free: serde + schemars only). clap CLI flags for `limit`
//! / `cursor` stay in each domain's args struct and map into [`PageParams`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default page size when the caller does not specify `limit`.
pub const DEFAULT_LIMIT: u32 = 50;
/// Hard upper bound on page size; larger `limit` values are clamped to this.
pub const MAX_LIMIT: u32 = 200;

/// Pagination request, embedded in any `*.list` args struct.
///
/// Both fields are optional: omit-all = first page at [`DEFAULT_LIMIT`].
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PageParams {
    /// Max items to return this page. Clamped to `[1, MAX_LIMIT]`; defaults to
    /// [`DEFAULT_LIMIT`] when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `next_cursor`. Omit for the first
    /// page. Treat as a blob — its contents are an implementation detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl PageParams {
    /// Effective page size: caller's `limit` clamped to `[1, MAX_LIMIT]`, or
    /// [`DEFAULT_LIMIT`].
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize
    }

    /// Decode the opaque cursor into a start offset. An absent or unparseable
    /// cursor starts at 0 (fail-soft: a garbage cursor yields the first page
    /// rather than an error, so a stale client never hard-fails a list).
    pub fn start_offset(&self) -> usize {
        self.cursor
            .as_deref()
            .and_then(decode_offset_cursor)
            .unwrap_or(0)
    }
}

/// One page of a `list` response. `items` holds the thin rows; `next_cursor` is
/// `Some` when more rows remain (pass it back to fetch the next page) and `None`
/// on the last page.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    /// Rows for this page (already limited to the effective page size).
    pub items: Vec<T>,
    /// Opaque cursor for the next page, or `None` if this is the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total number of rows across all pages, when cheap to compute (in-memory
    /// lists). `None` when the source can't count without scanning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

impl<T> Page<T> {
    /// Paginate an already-materialised, stably-ordered slice of rows in memory.
    /// Use for computed lists (catalog joins, loaded-plugin rosters, etc.). For
    /// large DB-backed tables prefer a keyset query and [`Page::keyset`].
    ///
    /// Caller MUST pass rows in a stable, deterministic order so offsets are
    /// consistent across pages.
    pub fn from_slice(all: Vec<T>, params: &PageParams) -> Self {
        let total = all.len() as u64;
        let start = params.start_offset().min(all.len());
        let limit = params.effective_limit();
        let end = start.saturating_add(limit).min(all.len());
        let has_more = end < all.len();
        let items: Vec<T> = all.into_iter().skip(start).take(limit).collect();
        Page {
            items,
            next_cursor: has_more.then(|| encode_offset_cursor(end)),
            total: Some(total),
        }
    }

    /// Build a page from rows a caller already fetched via a keyset/limit query.
    /// `next_cursor` should be the opaque token for the row AFTER the last one
    /// returned (or `None` if the source is exhausted). `total` is `None` unless
    /// the source can count cheaply.
    pub fn keyset(items: Vec<T>, next_cursor: Option<String>, total: Option<u64>) -> Self {
        Page {
            items,
            next_cursor,
            total,
        }
    }
}

/// Encode an offset as an opaque cursor. Base64url of `off:<n>` so the wire form
/// is not an obviously-mutable integer clients might hand-craft.
pub fn encode_offset_cursor(offset: usize) -> String {
    utils::encoding::base64url_encode(format!("off:{offset}").as_bytes())
}

/// Decode an opaque offset cursor produced by [`encode_offset_cursor`]. Returns
/// `None` for anything that does not parse (caller treats that as offset 0).
pub fn decode_offset_cursor(cursor: &str) -> Option<usize> {
    let raw = utils::encoding::base64url_decode(cursor).ok()?;
    let s = std::str::from_utf8(&raw).ok()?;
    s.strip_prefix("off:")?.parse::<usize>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_limit_clamps() {
        assert_eq!(
            PageParams::default().effective_limit(),
            DEFAULT_LIMIT as usize
        );
        assert_eq!(
            PageParams {
                limit: Some(0),
                cursor: None
            }
            .effective_limit(),
            1
        );
        assert_eq!(
            PageParams {
                limit: Some(9999),
                cursor: None
            }
            .effective_limit(),
            MAX_LIMIT as usize
        );
    }

    #[test]
    fn cursor_roundtrip() {
        let c = encode_offset_cursor(50);
        assert_eq!(decode_offset_cursor(&c), Some(50));
        assert_eq!(decode_offset_cursor("garbage"), None);
    }

    #[test]
    fn paginate_slice_walks_all_pages() {
        let all: Vec<u32> = (0..125).collect();
        let p1 = Page::from_slice(
            all.clone(),
            &PageParams {
                limit: Some(50),
                cursor: None,
            },
        );
        assert_eq!(p1.items.len(), 50);
        assert_eq!(p1.total, Some(125));
        assert!(p1.next_cursor.is_some());

        let p2 = Page::from_slice(
            all.clone(),
            &PageParams {
                limit: Some(50),
                cursor: p1.next_cursor.clone(),
            },
        );
        assert_eq!(p2.items, (50..100).collect::<Vec<_>>());
        assert!(p2.next_cursor.is_some());

        let p3 = Page::from_slice(
            all,
            &PageParams {
                limit: Some(50),
                cursor: p2.next_cursor.clone(),
            },
        );
        assert_eq!(p3.items, (100..125).collect::<Vec<_>>());
        assert_eq!(p3.next_cursor, None);
    }
}
