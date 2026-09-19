//! Scheduled diff-only reconcile (#494 epic, #503).
//!
//! The completeness reconcile (#496) computes ALL current gaps. Run on a
//! schedule, it must not re-emit a want-request every tick for a gap already
//! requested — that is exactly the tight-loop churn the Libation 6h-loop
//! throttle incident taught us to avoid. This layer diffs the current gaps
//! against a persisted "already requested" set and returns only the NEW gaps,
//! so a scheduled run acts once per genuinely-new gap.
//!
//! Pacing itself is an orca `schedule` row on a throttle-safe cadence
//! (daily/weekly, never a tight loop); this module is the pure diff the
//! scheduled job runs. Pairs with the [`crate::search_driver`] pacing gate.
//!
//! Pure + generic. Gap identity is reduced to a stable [`identity_key`] so the
//! "seen" set survives across runs regardless of which id vocabulary a backend
//! reports.

use crate::MediaIdentity;
use crate::identity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A stable, canonical string key for a work, so the persisted "already
/// requested" set matches across runs. Prefers the lowest canonical external id
/// (sorted for determinism); else series `name#sequence`; else `title|year`.
pub fn identity_key(id: &MediaIdentity) -> String {
    let mut ext: Vec<String> = id
        .external_ids
        .iter()
        .map(|e| {
            let c = identity::canonicalize(e);
            format!("{}:{}", c.source, c.id)
        })
        .collect();
    if !ext.is_empty() {
        ext.sort();
        return ext.remove(0);
    }
    if let Some(s) = &id.series
        && let Some(seq) = &s.sequence
    {
        return format!(
            "series:{}#{}",
            s.name.trim().to_lowercase(),
            seq.trim().to_lowercase()
        );
    }
    format!(
        "title:{}|{}",
        id.title.trim().to_lowercase(),
        id.year.map(|y| y.to_string()).unwrap_or_default()
    )
}

/// The diff of current gaps against the persisted "already requested" set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResult {
    /// Gaps not previously requested — the ONLY ones a scheduled run should act on.
    pub new_gaps: Vec<MediaIdentity>,
    /// Keys of ALL current gaps. The caller persists THIS as the next "already
    /// requested" set — replacing (not unioning) it, so a gap that is later
    /// filled drops out and can be re-requested if it ever regresses, while
    /// still-open gaps are never re-emitted.
    pub current_keys: Vec<String>,
}

/// Diff current gaps against what was already requested. Returns the new gaps
/// (to act on) and the full current key set (to persist for next run).
pub fn diff_new_gaps(
    current_gaps: &[MediaIdentity],
    already_requested: &BTreeSet<String>,
) -> DiffResult {
    let mut new_gaps = Vec::new();
    let mut current_keys = Vec::with_capacity(current_gaps.len());
    for g in current_gaps {
        let k = identity_key(g);
        if !already_requested.contains(&k) {
            new_gaps.push(g.clone());
        }
        current_keys.push(k);
    }
    DiffResult {
        new_gaps,
        current_keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExternalId, SeriesRef};

    fn isbn(title: &str, isbn: &str) -> MediaIdentity {
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

    #[test]
    fn key_is_canonical_across_id_formats() {
        let a = isbn("Dune", "9780441172719");
        let b = isbn("dune", "978-0-441-17271-9");
        assert_eq!(identity_key(&a), identity_key(&b));
    }

    #[test]
    fn key_falls_back_to_series_then_title() {
        let s = MediaIdentity {
            title: "Words of Radiance".into(),
            year: None,
            external_ids: vec![],
            series: Some(SeriesRef {
                name: "Stormlight".into(),
                sequence: Some("2".into()),
            }),
        };
        assert_eq!(identity_key(&s), "series:stormlight#2");
        let t = MediaIdentity {
            title: "Loose".into(),
            year: Some(2020),
            external_ids: vec![],
            series: None,
        };
        assert_eq!(identity_key(&t), "title:loose|2020");
    }

    #[test]
    fn only_new_gaps_are_returned() {
        let gaps = vec![isbn("A", "1"), isbn("B", "2"), isbn("C", "3")];
        let mut seen = BTreeSet::new();
        seen.insert(identity_key(&isbn("B", "2")));
        let r = diff_new_gaps(&gaps, &seen);
        let titles: Vec<_> = r.new_gaps.iter().map(|g| g.title.clone()).collect();
        assert_eq!(titles, vec!["A".to_string(), "C".to_string()]);
        assert_eq!(r.current_keys.len(), 3);
    }

    #[test]
    fn filled_gap_drops_from_persisted_set_and_can_requeue() {
        // Run 1: A and B are gaps, both new → requested; persist current_keys.
        let run1 = diff_new_gaps(&[isbn("A", "1"), isbn("B", "2")], &BTreeSet::new());
        assert_eq!(run1.new_gaps.len(), 2);
        let seen: BTreeSet<String> = run1.current_keys.into_iter().collect();

        // Run 2: A got filled (no longer a gap); B still a gap → NOT re-emitted.
        let run2 = diff_new_gaps(&[isbn("B", "2")], &seen);
        assert!(run2.new_gaps.is_empty());
        let seen2: BTreeSet<String> = run2.current_keys.into_iter().collect();

        // Run 3: A regressed (gap again). Because we REPLACE the set each run,
        // A's key is gone from seen2 → it re-requests. B still suppressed.
        let run3 = diff_new_gaps(&[isbn("A", "1"), isbn("B", "2")], &seen2);
        let titles: Vec<_> = run3.new_gaps.iter().map(|g| g.title.clone()).collect();
        assert_eq!(titles, vec!["A".to_string()]);
    }

    #[test]
    fn no_gaps_is_empty_diff() {
        let r = diff_new_gaps(&[], &BTreeSet::new());
        assert!(r.new_gaps.is_empty());
        assert!(r.current_keys.is_empty());
    }
}
