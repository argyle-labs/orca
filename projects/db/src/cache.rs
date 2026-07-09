//! In-memory read cache for hot DB queries.
//!
//! Sits ABOVE the `pool` module. Pattern:
//!
//! ```text
//! caller → cache::get_or_load(key, || pool::with_conn(load_from_db)) → value
//!          ↑ µs on hit                ↑ ms on miss (still pooled — no KDF)
//! ```
//!
//! Bounded by TTL + max_capacity so it cannot grow unbounded. On write,
//! the corresponding cache entry is invalidated — the next read repopulates
//! from the now-current DB row.
//!
//! Why moka: production-grade Rust cache. TTL eviction, LRU bound, no
//! background thread (eviction is amortized into reads). Sync API matches
//! how `pool::with_conn` is shaped.

use moka::sync::Cache;
use std::sync::LazyLock;
use std::time::Duration;

/// Latest `host_status` payload per peer. Hot read path — every `pod.list`
/// builds DTOs from this. TTL short enough that a write-then-read race
/// resolves within one tick of the writer cadence (2s fast, 30s slow).
pub static HOST_STATUS_LATEST: LazyLock<Cache<String, HostStatusEntry>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(1024) // fleet size cap
        .time_to_live(Duration::from_secs(2))
        .build()
});

/// Per-peer runtime spec (version, target, channel). Lower-cardinality,
/// changes only on update. Longer TTL — the host_status writer invalidates
/// explicitly via `invalidate_peer_runtime`.
pub static PEER_RUNTIME_SPEC: LazyLock<Cache<String, RuntimeSpecEntry>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(1024)
        .time_to_live(Duration::from_secs(15))
        .build()
});

/// Rendered web-provider responses, keyed by `"{provider}:{method} {path}"`.
/// A web plugin serves static-ish assets over an out-of-process socket; caching
/// the rendered bytes keeps the hot asset path off the round-trip. Short TTL so
/// a dev rebuild's changed assets surface quickly; asset URLs are content-hashed
/// in prod so staleness is a non-issue there.
pub static WEB_RESPONSE: LazyLock<Cache<String, WebResponseEntry>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(2048)
        .time_to_live(Duration::from_secs(5))
        .build()
});

#[derive(Clone)]
pub struct WebResponseEntry {
    /// Serialized `contract::web::WebResponse` JSON. Kept as a string so `db`
    /// takes no dependency on `contract` — the server (de)serializes at the seam.
    pub response_json: String,
}

/// Invalidate every cached web response for `provider` (e.g. on plugin reload).
pub fn invalidate_web_provider(provider: &str) {
    let needle = format!("{provider}:");
    if let Err(e) = WEB_RESPONSE.invalidate_entries_if(move |k, _| k.starts_with(&needle)) {
        tracing::warn!("web cache invalidation for '{provider}' skipped: {e}");
    }
}

#[derive(Clone)]
pub struct HostStatusEntry {
    pub payload_json: String,
    pub snapshot_at_unix: i64,
}

#[derive(Clone)]
pub struct RuntimeSpecEntry {
    pub version: Option<String>,
    pub target: Option<String>,
    pub frontend: Option<String>,
    pub mode: Option<String>,
    pub channel: Option<String>,
}

/// Invalidate a peer's `host_status` cache entry. Called from the writer
/// after a successful INSERT so the next read sees the fresh row.
pub fn invalidate_host_status(peer_id: &str) {
    HOST_STATUS_LATEST.invalidate(peer_id);
}

/// Invalidate a peer's runtime-spec cache entry.
pub fn invalidate_peer_runtime(peer_id: &str) {
    PEER_RUNTIME_SPEC.invalidate(peer_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_status_roundtrips_then_evicts() {
        let key = "test-roundtrip".to_string();
        HOST_STATUS_LATEST.insert(
            key.clone(),
            HostStatusEntry {
                payload_json: "{}".into(),
                snapshot_at_unix: 1,
            },
        );
        assert!(HOST_STATUS_LATEST.get(&key).is_some());
        invalidate_host_status(&key);
        // moka is eventually consistent on invalidation; force-sync.
        HOST_STATUS_LATEST.run_pending_tasks();
        assert!(HOST_STATUS_LATEST.get(&key).is_none());
    }

    #[test]
    fn runtime_spec_invalidation_clears_entry() {
        let key = "test-runtime".to_string();
        PEER_RUNTIME_SPEC.insert(
            key.clone(),
            RuntimeSpecEntry {
                version: Some("0.0.7".into()),
                target: None,
                frontend: None,
                mode: None,
                channel: None,
            },
        );
        assert!(PEER_RUNTIME_SPEC.get(&key).is_some());
        invalidate_peer_runtime(&key);
        PEER_RUNTIME_SPEC.run_pending_tasks();
        assert!(PEER_RUNTIME_SPEC.get(&key).is_none());
    }
}
