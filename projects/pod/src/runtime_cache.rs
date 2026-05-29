//! In-memory per-peer runtime snapshot. Filled by the host_status puller
//! after each successful `system.runtime-detail` fetch; read by
//! `services::pod::list_enriched_impl` when building the `pod.list` DTOs.
//!
//! No persistence — the puller refills the cache within one sync tick after
//! a daemon restart (~60s). Storing this in `host_status.payload_json` would
//! couple the runtime fields to the OS snapshot's schema; a sibling table is
//! the right long-term home but the in-memory cache unblocks the UI today
//! without a migration.
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

#[derive(Clone, Default)]
pub struct RuntimeFields {
    pub version: Option<String>,
    pub target: Option<String>,
    pub frontend: Option<String>,
    pub mode: Option<String>,
    pub channel: Option<String>,
    pub pinned_to: Option<String>,
}

static CACHE: LazyLock<RwLock<HashMap<String, RuntimeFields>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn put(peer_id: &str, fields: RuntimeFields) {
    if let Ok(mut g) = CACHE.write() {
        g.insert(peer_id.to_string(), fields);
    }
}

pub fn get(peer_id: &str) -> Option<RuntimeFields> {
    CACHE.read().ok().and_then(|g| g.get(peer_id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_roundtrips() {
        put(
            "peer.test-roundtrip",
            RuntimeFields {
                version: Some("0.0.5-rc.2".into()),
                target: Some("aarch64-apple-darwin".into()),
                frontend: Some("embedded".into()),
                mode: Some("daemon".into()),
                channel: Some("rc".into()),
                pinned_to: None,
            },
        );
        let got = get("peer.test-roundtrip").expect("cache hit");
        assert_eq!(got.version.as_deref(), Some("0.0.5-rc.2"));
        assert_eq!(got.channel.as_deref(), Some("rc"));
        assert!(got.pinned_to.is_none());
    }

    #[test]
    fn miss_returns_none() {
        assert!(get("peer.never-inserted").is_none());
    }
}
