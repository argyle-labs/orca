//! Blocklist / anti-retry-storm (#496 epic, #501).
//!
//! A release that can never succeed (dead usenet article, dead torrent, a
//! wrong-format grab the importer keeps rejecting) otherwise re-grabs forever —
//! the "Rhythm of War" class this session hit was ~60 identical Failed rows
//! churning against the same release. This detects that pattern from the
//! requester's failure history and recommends blocklisting the offending
//! release so it stops re-grabbing; the caller applies it via
//! [`crate::seam::Requester::blocklist`].
//!
//! Pure + generic: it counts failures per `(work, release)` and thresholds. No
//! timestamps needed — repeated identical Failed rows ARE the signal, and the
//! existing failed rows serve as the de-facto blocklist once the release is
//! skipped. Works for any requester.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One failed acquisition attempt as read from the requester's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRow {
    /// The work this attempt was for (normalized title / identity key).
    pub identity_key: String,
    /// The specific release that failed (nzb id, torrent guid, release title).
    pub release_id: String,
}

/// How many identical-release failures before we blocklist it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlocklistPolicy {
    /// Failures of the SAME release at/above which it should be blocklisted.
    pub max_failures: u32,
}

impl Default for BlocklistPolicy {
    fn default() -> Self {
        // Three strikes: enough to distinguish a transient hiccup from a release
        // that will never succeed, without letting a storm build.
        BlocklistPolicy { max_failures: 3 }
    }
}

/// A release the detector recommends blocklisting, with its observed failure
/// count so the caller can log/report why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlocklistRecommendation {
    pub identity_key: String,
    pub release_id: String,
    pub failures: u32,
}

/// From a requester's failure history, recommend blocklisting every release
/// whose failure count meets the policy threshold. Deterministic order (by
/// identity then release) so output is stable for logging/diffing.
pub fn recommend_blocklist(
    rows: &[FailureRow],
    policy: &BlocklistPolicy,
) -> Vec<BlocklistRecommendation> {
    // Count failures per (identity_key, release_id). BTreeMap → stable order.
    let mut counts: BTreeMap<(&str, &str), u32> = BTreeMap::new();
    for r in rows {
        *counts
            .entry((r.identity_key.as_str(), r.release_id.as_str()))
            .or_insert(0) += 1;
    }
    let threshold = policy.max_failures.max(1);
    counts
        .into_iter()
        .filter(|(_, n)| *n >= threshold)
        .map(
            |((identity_key, release_id), failures)| BlocklistRecommendation {
                identity_key: identity_key.to_string(),
                release_id: release_id.to_string(),
                failures,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(identity: &str, release: &str) -> FailureRow {
        FailureRow {
            identity_key: identity.into(),
            release_id: release.into(),
        }
    }

    #[test]
    fn churning_release_is_recommended_for_blocklist() {
        // Rhythm-of-War class: the same release fails over and over.
        let rows: Vec<FailureRow> = (0..60)
            .map(|_| fail("rhythm of war", "ROW.RETAIL.EPUB-DEADGRP"))
            .collect();
        let recs = recommend_blocklist(&rows, &BlocklistPolicy::default());
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].release_id, "ROW.RETAIL.EPUB-DEADGRP");
        assert_eq!(recs[0].failures, 60);
    }

    #[test]
    fn transient_failures_below_threshold_are_left_alone() {
        let rows = vec![
            fail("dune", "rel-a"),
            fail("dune", "rel-a"), // 2 < 3
        ];
        assert!(recommend_blocklist(&rows, &BlocklistPolicy::default()).is_empty());
    }

    #[test]
    fn distinct_releases_counted_separately_not_summed() {
        // Two different releases for one work, each failing twice: neither hits
        // the threshold — we must not sum across releases.
        let rows = vec![
            fail("book", "rel-a"),
            fail("book", "rel-a"),
            fail("book", "rel-b"),
            fail("book", "rel-b"),
        ];
        assert!(recommend_blocklist(&rows, &BlocklistPolicy::default()).is_empty());
    }

    #[test]
    fn multiple_bad_releases_all_reported_in_stable_order() {
        let mut rows = Vec::new();
        for _ in 0..4 {
            rows.push(fail("z-work", "bad-z"));
        }
        for _ in 0..3 {
            rows.push(fail("a-work", "bad-a"));
        }
        let recs = recommend_blocklist(&rows, &BlocklistPolicy::default());
        assert_eq!(recs.len(), 2);
        // BTreeMap key order → a-work before z-work.
        assert_eq!(recs[0].identity_key, "a-work");
        assert_eq!(recs[1].identity_key, "z-work");
    }
}
