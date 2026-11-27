//! Completeness reconcile — the first slice of the epic #494 brain (#496).
//!
//! Given a canonical *expected* set for a series/work (the reading order) and
//! the *owned* library state read from a [`LibraryServer`], compute the missing
//! volumes and emit want-requests to a [`Requester`]. This replaces the manual
//! metadata.db-vs-canonical diffing we hand-drove and is GENERIC across every
//! requester + library backend — it dispatches over the two seams (#495), never
//! over a backend API.
//!
//! Split in two so the decision logic is pure and unit-testable:
//! - [`compute_gaps`] — pure identity diff (expected − owned). No I/O.
//! - [`reconcile_series`] — the async driver: reads owned state over the
//!   `LibraryServer` seam, diffs, and (unless `dry_run`) adds each gap over the
//!   `Requester` seam.
//!
//! Performance (standing directive): [`compute_gaps`] indexes owned state ONCE
//! into hash sets, so matching is ~O(expected) lookups, not O(expected×owned).
//! The driver reads only the requested [`Scope`] — a series-scoped read, not a
//! full-library rescan.

use crate::identity;
use crate::seam::{AddRequest, LibraryServer, QualityPolicy, Requester, Scope};
use crate::search_driver::Budget;
use crate::{MediaError, MediaIdentity, MediaType, RequestStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// ── Identity matching ─────────────────────────────────────────────────────────

/// Normalized match keys for one identity: canonical external ids, a
/// series (name, sequence) key, and a (title, year) fallback. Two identities
/// are "the same work" if ANY key coincides — external id is strongest, series
/// position next, title/year last. Building keys once lets the diff index them.
struct MatchKeys {
    ext: Vec<(String, String)>,
    series: Option<(String, String)>,
    title_year: (String, Option<u16>),
}

impl MatchKeys {
    fn of(id: &MediaIdentity) -> Self {
        let ext = id
            .external_ids
            .iter()
            .map(|e| {
                let c = identity::canonicalize(e);
                (c.source, c.id)
            })
            .collect();
        let series = id.series.as_ref().and_then(|s| {
            s.sequence
                .as_ref()
                .map(|seq| (s.name.trim().to_lowercase(), seq.trim().to_lowercase()))
        });
        let title_year = (id.title.trim().to_lowercase(), id.year);
        MatchKeys {
            ext,
            series,
            title_year,
        }
    }
}

/// An index over the OWNED identities, so each expected identity is matched with
/// a few hash lookups instead of a linear scan.
struct OwnedIndex {
    ext: HashSet<(String, String)>,
    series: HashSet<(String, String)>,
    title_year: HashSet<(String, Option<u16>)>,
}

impl OwnedIndex {
    fn build(owned: &[MediaIdentity]) -> Self {
        let mut ext = HashSet::new();
        let mut series = HashSet::new();
        let mut title_year = HashSet::new();
        for id in owned {
            let k = MatchKeys::of(id);
            ext.extend(k.ext);
            if let Some(s) = k.series {
                series.insert(s);
            }
            title_year.insert(k.title_year);
        }
        OwnedIndex {
            ext,
            series,
            title_year,
        }
    }

    /// Is this expected identity satisfied by something owned?
    fn contains(&self, expected: &MediaIdentity) -> bool {
        let k = MatchKeys::of(expected);
        if k.ext.iter().any(|e| self.ext.contains(e)) {
            return true;
        }
        if let Some(s) = &k.series
            && self.series.contains(s)
        {
            return true;
        }
        self.title_year.contains(&k.title_year)
    }
}

/// Pure gap computation: the expected identities NOT satisfied by any owned
/// identity, preserving expected order. This is the decision core #496 exists
/// for — deterministic and I/O-free so it is fully unit-testable.
pub fn compute_gaps(expected: &[MediaIdentity], owned: &[MediaIdentity]) -> Vec<MediaIdentity> {
    let idx = OwnedIndex::build(owned);
    expected
        .iter()
        .filter(|e| !idx.contains(e))
        .cloned()
        .collect()
}

// ── Report + driver ───────────────────────────────────────────────────────────

/// Why a gap was not requested this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    BudgetExhausted,
    /// The requester reported it cannot request this gap; `error` says why.
    Unrequestable,
    /// The run's deadline passed before this gap was requested.
    DeadlineReached,
}

/// What one gap-add attempt did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapAction {
    pub identity: MediaIdentity,
    /// Whether the want-request was issued; false on a dry run or when skipped.
    #[serde(default)]
    pub sent: bool,
    /// Present when we actually issued a want-request (not a dry run).
    #[serde(default)]
    pub status: Option<RequestStatus>,
    /// Present when the add failed (adapter error), for surfacing without
    /// aborting the whole reconcile.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub skipped: Option<SkipReason>,
}

impl GapAction {
    fn unsent(gap: &MediaIdentity, skipped: Option<SkipReason>, error: Option<String>) -> Self {
        GapAction {
            identity: gap.clone(),
            sent: false,
            status: None,
            error,
            skipped,
        }
    }
}

/// The outcome of a series reconcile: the counts, the computed gaps, and what
/// each want-request did (or would do, on a dry run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileOutcome {
    pub media_type: MediaType,
    pub server: String,
    pub requester: String,
    pub expected: usize,
    pub owned: usize,
    /// Missing volumes (expected − owned), in expected order.
    pub gaps: Vec<MediaIdentity>,
    /// One entry per gap; on a dry run nothing is `sent`.
    #[serde(default)]
    pub actions: Vec<GapAction>,
    /// Gaps the run's [`Budget`] covered (requested, or would be on a dry run).
    #[serde(default)]
    pub selected: usize,
    /// Requestable gaps left for a later run once the budget ran out.
    #[serde(default)]
    pub remaining: usize,
    pub dry_run: bool,
}

/// Reconcile one canonical/series expected set against a library server +
/// requester: read owned state for `scope`, diff, and (unless `dry_run`) emit a
/// want-request for each gap under `quality`, within `budget`.
///
/// Add failures are captured per-gap in the outcome rather than aborting, so one
/// bad volume never blocks the rest of the series. Reading owned state is the
/// only hard-failure path (nothing to diff against).
pub async fn reconcile_series(
    expected: &[MediaIdentity],
    server: &dyn LibraryServer,
    requester: &dyn Requester,
    quality: &QualityPolicy,
    scope: &Scope,
    budget: &mut Budget,
    dry_run: bool,
) -> Result<ReconcileOutcome, MediaError> {
    let state = server.library_state(scope).await?;
    let owned: Vec<MediaIdentity> = state.items.into_iter().map(|e| e.identity).collect();
    let gaps = compute_gaps(expected, &owned);
    let left = budget.left();
    let mut requestable = 0;
    let mut selected = 0;
    let mut deadline_hit = false;

    let mut actions = Vec::with_capacity(gaps.len());
    for gap in &gaps {
        if let Some(why) = requester.unrequestable(gap) {
            actions.push(GapAction::unsent(
                gap,
                Some(SkipReason::Unrequestable),
                Some(why),
            ));
            continue;
        }
        requestable += 1;
        if requestable > left {
            actions.push(GapAction::unsent(
                gap,
                Some(SkipReason::BudgetExhausted),
                None,
            ));
            continue;
        }
        if dry_run {
            selected += 1;
            actions.push(GapAction::unsent(gap, None, None));
            continue;
        }
        if !deadline_hit && !budget.expired() {
            budget.pace().await;
        }
        deadline_hit = deadline_hit || budget.expired();
        if deadline_hit {
            actions.push(GapAction::unsent(
                gap,
                Some(SkipReason::DeadlineReached),
                None,
            ));
            continue;
        }
        budget.spend(1);
        selected += 1;
        let req = AddRequest {
            identity: gap.clone(),
            quality: quality.clone(),
            search_now: true,
        };
        let (status, error) = match requester.add(&req).await {
            Ok(status) => (Some(status), None),
            Err(e) => (None, Some(e.to_string())),
        };
        actions.push(GapAction {
            identity: gap.clone(),
            sent: true,
            status,
            error,
            skipped: None,
        });
    }
    if dry_run {
        budget.spend(selected);
    }
    let remaining = requestable - selected;

    Ok(ReconcileOutcome {
        media_type: requester.media_type(),
        server: server.name().to_string(),
        requester: requester.name().to_string(),
        expected: expected.len(),
        owned: owned.len(),
        actions,
        selected,
        remaining,
        gaps,
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::{Collection, LibraryEntry, LibraryState, Metadata, RequestState};
    use crate::search_driver::SearchPolicy;
    use crate::{ExternalId, MediaMutation, SeriesRef};
    use derive::orca_async;

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

    fn seq(name: &str, title: &str, sequence: &str) -> MediaIdentity {
        MediaIdentity {
            title: title.into(),
            year: None,
            external_ids: vec![],
            series: Some(SeriesRef {
                name: name.into(),
                sequence: Some(sequence.into()),
            }),
        }
    }

    #[test]
    fn gap_by_external_id_ignores_id_format_and_title_casing() {
        // Owned carries hyphenated ISBN-13; expected the bare form — canonicalize
        // makes them match, so it is NOT a gap.
        let expected = vec![
            isbn("Dune", "9780441172719"),
            isbn("Dune Messiah", "9780593098233"),
        ];
        let owned = vec![isbn("dune", "978-0-441-17271-9")];
        let gaps = compute_gaps(&expected, &owned);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].title, "Dune Messiah");
    }

    #[test]
    fn gap_by_series_sequence_when_no_external_id() {
        // A canonical reading order often has series+sequence but no ISBN; the
        // library has the sequence too. Match on that, don't false-gap.
        let expected = vec![
            seq("Stormlight", "The Way of Kings", "1"),
            seq("Stormlight", "Words of Radiance", "2"),
            seq("Stormlight", "Oathbringer", "3"),
        ];
        let owned = vec![
            seq("stormlight", "way of kings", "1"),
            seq("Stormlight", "wor", "2"),
        ];
        let gaps = compute_gaps(&expected, &owned);
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            gaps[0].series.as_ref().unwrap().sequence.as_deref(),
            Some("3")
        );
    }

    #[test]
    fn nothing_owned_is_all_gaps_empty_expected_is_none() {
        let expected = vec![isbn("A", "1"), isbn("B", "2")];
        assert_eq!(compute_gaps(&expected, &[]).len(), 2);
        assert_eq!(compute_gaps(&[], &expected).len(), 0);
    }

    // Fakes exercising the driver over the two seams.
    struct Srv {
        owned: Vec<MediaIdentity>,
    }
    #[orca_async]
    impl LibraryServer for Srv {
        fn name(&self) -> &str {
            "calibre"
        }
        fn media_type(&self) -> MediaType {
            MediaType::Ebooks
        }
        async fn scan(&self, _s: &Scope) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
        async fn collection_get(&self, _id: &str) -> Result<Collection, MediaError> {
            Err(MediaError::NotFound("x".into()))
        }
        async fn collection_set(&self, _c: &Collection) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
        async fn metadata_get(&self, id: &MediaIdentity) -> Result<Metadata, MediaError> {
            Ok(Metadata {
                identity: id.clone(),
                fields: Default::default(),
            })
        }
        async fn metadata_set(&self, _m: &Metadata) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
        async fn library_state(&self, _s: &Scope) -> Result<LibraryState, MediaError> {
            Ok(LibraryState {
                item_count: self.owned.len() as u64,
                items: self
                    .owned
                    .iter()
                    .map(|i| LibraryEntry {
                        identity: i.clone(),
                        id: None,
                        editions: vec![],
                    })
                    .collect(),
            })
        }
    }

    struct Req {
        added: std::sync::Mutex<Vec<String>>,
        fail: Option<&'static str>,
        unrequestable: Option<&'static str>,
    }

    fn req() -> Req {
        Req {
            added: std::sync::Mutex::new(vec![]),
            fail: None,
            unrequestable: None,
        }
    }
    #[orca_async]
    impl Requester for Req {
        fn name(&self) -> &str {
            "lazylibrarian"
        }
        fn media_type(&self) -> MediaType {
            MediaType::Ebooks
        }
        async fn add(&self, req: &AddRequest) -> Result<RequestStatus, MediaError> {
            self.added.lock().unwrap().push(req.identity.title.clone());
            if self.fail == Some(req.identity.title.as_str()) {
                return Err(MediaError::Transport("unqueueable".into()));
            }
            Ok(RequestStatus {
                identity: req.identity.clone(),
                state: RequestState::Wanted,
                active_release: None,
                detail: None,
            })
        }
        fn unrequestable(&self, id: &MediaIdentity) -> Option<String> {
            (self.unrequestable == Some(id.title.as_str())).then(|| "no backend id".into())
        }
        async fn search(&self, _id: &MediaIdentity) -> Result<Vec<crate::SearchHit>, MediaError> {
            Ok(vec![])
        }
        async fn status(&self, id: &MediaIdentity) -> Result<RequestStatus, MediaError> {
            Ok(RequestStatus {
                identity: id.clone(),
                state: RequestState::Unknown,
                active_release: None,
                detail: None,
            })
        }
        async fn blocklist(
            &self,
            id: &MediaIdentity,
            _r: &crate::ReleaseRef,
        ) -> Result<RequestStatus, MediaError> {
            Ok(RequestStatus {
                identity: id.clone(),
                state: RequestState::Searching,
                active_release: None,
                detail: None,
            })
        }
        async fn import_trigger(&self, _id: &MediaIdentity) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn driver_emits_want_requests_only_for_gaps() {
        let expected = vec![isbn("A", "1"), isbn("B", "2"), isbn("C", "3")];
        let srv = Srv {
            owned: vec![isbn("B", "2")],
        };
        let req = req();
        let out = reconcile_series(
            &expected,
            &srv,
            &req,
            &QualityPolicy::default(),
            &Scope::All,
            &mut Budget::unlimited(&SearchPolicy::default()),
            false,
        )
        .await
        .unwrap();

        assert_eq!(out.expected, 3);
        assert_eq!(out.owned, 1);
        assert_eq!(out.gaps.len(), 2);
        assert_eq!(out.actions.len(), 2);
        assert!(out.actions.iter().all(|a| a.sent && a.error.is_none()));
        let added = req.added.lock().unwrap().clone();
        assert_eq!(added, vec!["A".to_string(), "C".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn dry_run_computes_gaps_but_emits_nothing() {
        let expected = vec![isbn("A", "1"), isbn("B", "2")];
        let srv = Srv { owned: vec![] };
        let req = req();
        let out = reconcile_series(
            &expected,
            &srv,
            &req,
            &QualityPolicy::default(),
            &Scope::All,
            &mut Budget::unlimited(&SearchPolicy::default()),
            true,
        )
        .await
        .unwrap();
        assert_eq!(out.gaps.len(), 2);
        assert_eq!(out.actions.len(), 2);
        assert!(out.actions.iter().all(|a| !a.sent && a.skipped.is_none()));
        assert!(req.added.lock().unwrap().is_empty());
    }

    fn paced(pace: std::time::Duration) -> SearchPolicy {
        SearchPolicy {
            request_pace_ms: pace.as_millis() as u64,
            ..SearchPolicy::default()
        }
    }

    async fn run(
        expected: &[MediaIdentity],
        req: &Req,
        budget: &mut Budget,
        dry_run: bool,
    ) -> ReconcileOutcome {
        reconcile_series(
            expected,
            &Srv { owned: vec![] },
            req,
            &QualityPolicy::default(),
            &Scope::All,
            budget,
            dry_run,
        )
        .await
        .unwrap()
    }

    #[test]
    fn budget_rejects_a_zero_limit() {
        let err = Budget::new(0, &SearchPolicy::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn limit_spans_series_and_reports_remaining() {
        let req = req();
        let mut budget = Budget::new(4, &SearchPolicy::default()).unwrap();
        let mut left = vec![];
        for s in ["X", "Y", "Z"] {
            let expected: Vec<_> = (1..=3)
                .map(|n| isbn(&format!("{s}{n}"), &format!("{s}{n}")))
                .collect();
            let out = run(&expected, &req, &mut budget, false).await;
            let skipped = out
                .actions
                .iter()
                .filter(|a| a.skipped == Some(SkipReason::BudgetExhausted))
                .count();
            assert_eq!(skipped, out.remaining);
            left.push(out.remaining);
        }
        assert_eq!(left, vec![0, 2, 3]);
        assert_eq!(budget.used(), 4);
        assert_eq!(req.added.lock().unwrap().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn dry_run_spends_budget_without_requesting() {
        let req = req();
        let mut budget = Budget::new(2, &SearchPolicy::default()).unwrap();
        let expected = vec![isbn("A", "1"), isbn("B", "2"), isbn("C", "3")];
        let out = run(&expected, &req, &mut budget, true).await;
        assert_eq!((out.selected, out.remaining), (2, 1));
        let skipped: Vec<_> = out.actions.iter().map(|a| a.skipped).collect();
        assert_eq!(skipped, vec![None, None, Some(SkipReason::BudgetExhausted)]);
        assert!(out.actions.iter().all(|a| !a.sent));
        assert!(req.added.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn failed_request_is_reported_and_the_run_continues() {
        let req = Req {
            fail: Some("B"),
            ..req()
        };
        let expected = vec![isbn("A", "1"), isbn("B", "2"), isbn("C", "3")];
        let out = run(
            &expected,
            &req,
            &mut Budget::unlimited(&SearchPolicy::default()),
            false,
        )
        .await;
        let failed: Vec<_> = out
            .actions
            .iter()
            .filter(|a| a.error.is_some())
            .map(|a| a.identity.title.as_str())
            .collect();
        assert_eq!(failed, vec!["B"]);
        assert_eq!(req.added.lock().unwrap().len(), 3);
    }

    #[test]
    fn outcome_without_budget_fields_still_deserializes() {
        let out: ReconcileOutcome = serde_json::from_str(
            r#"{"media_type":"comics","server":"s","requester":"r","expected":1,"owned":0,"gaps":[],"dry_run":true}"#,
        )
        .unwrap();
        assert_eq!((out.selected, out.remaining), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn pace_is_clamped_to_the_minimum() {
        let mut budget = Budget::new(2, &paced(std::time::Duration::ZERO)).unwrap();
        budget.spend(1);
        let start = tokio::time::Instant::now();
        budget.pace().await;
        assert_eq!(start.elapsed(), crate::search_driver::MIN_REQUEST_PACE);
    }

    #[tokio::test(start_paused = true)]
    async fn pacing_sleeps_between_requests_across_series_not_before_the_first() {
        let req = req();
        let pace = std::time::Duration::from_secs(30);
        let mut budget = Budget::new(100, &paced(pace)).unwrap();
        let start = tokio::time::Instant::now();
        run(&[isbn("A", "1"), isbn("B", "2")], &req, &mut budget, false).await;
        run(&[isbn("C", "3")], &req, &mut budget, false).await;
        assert_eq!(req.added.lock().unwrap().len(), 3);
        assert_eq!(start.elapsed(), pace * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn dry_run_never_sleeps() {
        let req = req();
        let mut budget = Budget::new(100, &paced(std::time::Duration::from_secs(30))).unwrap();
        let start = tokio::time::Instant::now();
        run(&[isbn("A", "1"), isbn("B", "2")], &req, &mut budget, true).await;
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn unrequestable_gaps_are_reported_without_spending_or_pacing() {
        let req = Req {
            unrequestable: Some("A"),
            ..req()
        };
        let pace = std::time::Duration::from_secs(30);
        let mut budget = Budget::new(1, &paced(pace)).unwrap();
        let start = tokio::time::Instant::now();
        let expected = vec![isbn("A", "1"), isbn("B", "2"), isbn("C", "3")];
        let out = run(&expected, &req, &mut budget, false).await;
        assert_eq!(*req.added.lock().unwrap(), vec!["B".to_string()]);
        assert_eq!((out.selected, out.remaining), (1, 1));
        assert_eq!(budget.used(), 1);
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
        let a = out
            .actions
            .iter()
            .find(|a| a.identity.title == "A")
            .unwrap();
        assert_eq!(a.skipped, Some(SkipReason::Unrequestable));
        assert_eq!(a.error.as_deref(), Some("no backend id"));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_stops_requesting_and_reports_the_rest() {
        let req = req();
        let pace = std::time::Duration::from_secs(30);
        let start = tokio::time::Instant::now();
        let mut budget = Budget::new(3, &paced(pace))
            .unwrap()
            .with_deadline(pace + pace / 2);
        let expected: Vec<_> = ["A", "B", "C", "D"].iter().map(|t| isbn(t, t)).collect();
        let out = run(&expected, &req, &mut budget, false).await;
        assert_eq!(start.elapsed(), pace + pace / 2);
        assert_eq!(
            *req.added.lock().unwrap(),
            vec!["A".to_string(), "B".to_string()]
        );
        assert_eq!((out.selected, out.remaining), (2, 2));
        assert_eq!(budget.used(), 2);
        let skipped: Vec<_> = out.actions.iter().map(|a| a.skipped).collect();
        assert_eq!(
            skipped,
            vec![
                None,
                None,
                Some(SkipReason::DeadlineReached),
                Some(SkipReason::BudgetExhausted)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn actions_follow_gap_order() {
        let req = Req {
            unrequestable: Some("A"),
            ..req()
        };
        let mut budget = Budget::new(1, &SearchPolicy::default()).unwrap();
        let expected = vec![isbn("A", "1"), isbn("B", "2"), isbn("C", "3")];
        let out = run(&expected, &req, &mut budget, false).await;
        let rows: Vec<_> = out
            .actions
            .iter()
            .map(|a| (a.identity.title.as_str(), a.skipped))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("A", Some(SkipReason::Unrequestable)),
                ("B", None),
                ("C", Some(SkipReason::BudgetExhausted)),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unrequestable_after_cap_or_deadline_is_still_unrequestable() {
        let req = Req {
            unrequestable: Some("X"),
            ..req()
        };
        let pace = std::time::Duration::from_secs(30);
        let skips =
            |out: &ReconcileOutcome| -> Vec<_> { out.actions.iter().map(|a| a.skipped).collect() };

        let mut capped = Budget::new(1, &paced(pace)).unwrap();
        let start = tokio::time::Instant::now();
        let gaps: Vec<_> = ["A", "B", "X"].iter().map(|t| isbn(t, t)).collect();
        let out = run(&gaps, &req, &mut capped, false).await;
        assert_eq!(
            skips(&out),
            vec![
                None,
                Some(SkipReason::BudgetExhausted),
                Some(SkipReason::Unrequestable)
            ]
        );
        assert_eq!(capped.used(), 1);
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);

        let mut timed = Budget::new(10, &paced(pace))
            .unwrap()
            .with_deadline(pace + pace / 2);
        let start = tokio::time::Instant::now();
        let gaps: Vec<_> = ["C", "D", "E", "X"].iter().map(|t| isbn(t, t)).collect();
        let out = run(&gaps, &req, &mut timed, false).await;
        assert_eq!(
            skips(&out),
            vec![
                None,
                None,
                Some(SkipReason::DeadlineReached),
                Some(SkipReason::Unrequestable)
            ]
        );
        assert_eq!(timed.used(), 2);
        assert_eq!(start.elapsed(), pace + pace / 2);
        assert!(!req.added.lock().unwrap().contains(&"X".to_string()));
    }
}
