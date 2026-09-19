//! Edition-fragmentation-aware verified status read (#496 epic, #497).
//!
//! LazyLibrarian records Wanted/Snatched/Downloading on a SIBLING edition row
//! (a Google-Books alt edition, a series-bundle row), NOT the canonical bookid.
//! Reading only the canonical row therefore reports a false `Skipped` /
//! "queuing failed" while a sibling edition is actively downloading. This
//! normalizer collapses all sibling rows for one work down to the SINGLE
//! best-progressed state, so a title reads acquired if ANY of its editions is.
//!
//! Pure + generic: it operates on raw `(key, state)` rows the adapter reads,
//! keyed by whatever "same work" grouping the adapter can produce (normalized
//! title, or a canonical identity key). The [`RequestState`] progress ranking is
//! the single source of truth for "best".

use crate::seam::RequestState;
use std::collections::BTreeMap;

/// Acquisition-progress rank: higher = more progressed / more authoritative.
/// `Imported` is the terminal win; `Failed`/`Skipped` outrank only `Unknown`,
/// so a real positive state on ANY sibling always wins over a negative one.
fn rank(state: RequestState) -> u8 {
    match state {
        RequestState::Imported => 7,
        RequestState::Downloading => 6,
        RequestState::Snatched => 5,
        RequestState::Searching => 4,
        RequestState::Wanted => 3,
        RequestState::Failed => 2,
        RequestState::Skipped => 1,
        RequestState::Unknown => 0,
    }
}

/// The best state across a set of sibling rows: the max by [`rank`]. Empty → the
/// caller decides; here an empty slice yields `Unknown`.
pub fn best_state(states: impl IntoIterator<Item = RequestState>) -> RequestState {
    states
        .into_iter()
        .max_by_key(|s| rank(*s))
        .unwrap_or(RequestState::Unknown)
}

/// One raw status row as read from the requester backend, before normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStatusRow {
    /// The "same work" grouping key the adapter produced (normalized title or a
    /// canonical identity key). Sibling editions share this key.
    pub key: String,
    pub state: RequestState,
}

/// Collapse raw rows to one normalized state per work-key: dedupe across sibling
/// editions and take the best-progressed state. This is the verified read
/// [`crate::seam::Requester::status`] returns after the adapter groups its rows —
/// so the sibling-edition trap can never leak past the seam (#497).
pub fn normalize(rows: &[RawStatusRow]) -> BTreeMap<String, RequestState> {
    let mut out: BTreeMap<String, RequestState> = BTreeMap::new();
    for row in rows {
        out.entry(row.key.clone())
            .and_modify(|cur| {
                if rank(row.state) > rank(*cur) {
                    *cur = row.state;
                }
            })
            .or_insert(row.state);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, state: RequestState) -> RawStatusRow {
        RawStatusRow {
            key: key.into(),
            state,
        }
    }

    #[test]
    fn sibling_snatched_beats_canonical_skipped() {
        // The exact trap: canonical row reads Skipped, a sibling edition is
        // Snatched. Must report Snatched for the work.
        let rows = vec![
            row("dark tower vii", RequestState::Skipped),
            row("dark tower vii", RequestState::Snatched),
            row("dark tower vii", RequestState::Failed),
        ];
        let n = normalize(&rows);
        assert_eq!(n["dark tower vii"], RequestState::Snatched);
    }

    #[test]
    fn imported_is_terminal_win() {
        let rows = vec![
            row("dune", RequestState::Downloading),
            row("dune", RequestState::Imported),
            row("dune", RequestState::Wanted),
        ];
        assert_eq!(normalize(&rows)["dune"], RequestState::Imported);
    }

    #[test]
    fn distinct_works_stay_separate() {
        let rows = vec![
            row("a", RequestState::Wanted),
            row("b", RequestState::Imported),
        ];
        let n = normalize(&rows);
        assert_eq!(n["a"], RequestState::Wanted);
        assert_eq!(n["b"], RequestState::Imported);
    }

    #[test]
    fn all_negative_rows_report_best_negative_not_unknown() {
        let rows = vec![
            row("x", RequestState::Skipped),
            row("x", RequestState::Failed),
        ];
        // Failed (2) outranks Skipped (1) — a failed acquisition is more
        // informative than a deliberately-skipped one.
        assert_eq!(normalize(&rows)["x"], RequestState::Failed);
    }

    #[test]
    fn best_state_empty_is_unknown() {
        assert_eq!(best_state(std::iter::empty()), RequestState::Unknown);
    }
}
