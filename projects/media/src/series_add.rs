//! Add series to a manager and want their missing items.
//!
//! A manager's add only queues the series; it shows up in the manager's list and
//! fills in its items in the background. [`add_series`] adds each new series,
//! waits until it is listed and its item count settles, then wants the gaps over
//! the [`Requester`] seam within one run [`Budget`]. A backend supplies only the
//! [`SeriesManager`] calls; the waits, caps and reporting live here.

use crate::reconcile::{GapAction, Requested, SkipReason, compute_gaps, request_gaps};
use crate::seam::{QualityPolicy, Requester};
use crate::search_driver::Budget;
use crate::{MediaError, MediaIdentity};
use derive::orca_async;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use utils::time::{Deadline, Polled, Probe, poll_until};

/// What a manager currently lists for one series.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesView {
    #[serde(default)]
    pub name: Option<String>,
    /// The item count the series' metadata source reports, when known.
    #[serde(default)]
    pub expected: Option<usize>,
    /// Every item the manager lists for the series, in order.
    #[serde(default)]
    pub items: Vec<MediaIdentity>,
    /// The listed items not to want (held, or excluded by the backend's policy).
    #[serde(default)]
    pub owned: Vec<MediaIdentity>,
}

/// A backend that tracks series and imports their item lists asynchronously.
#[orca_async]
pub trait SeriesManager: Send + Sync {
    fn name(&self) -> &str;
    /// Ids of every series the manager lists, in the form [`add_series`] is given.
    async fn listed(&self) -> Result<Vec<String>, MediaError>;
    async fn add(&self, id: &str) -> Result<(), MediaError>;
    async fn series(&self, id: &str) -> Result<SeriesView, MediaError>;
}

/// How long to wait for a new series to be listed and for its import to settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Poll {
    pub interval: Duration,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesAddPlan {
    /// Want the missing items, not just add the series.
    pub want: bool,
    /// New series one run may attempt; listed series do not count.
    pub max_new: usize,
    pub poll: Poll,
    pub quality: QualityPolicy,
}

/// Why a series was left for a later run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pending {
    /// Not attempted: the run's `max_new` was reached.
    MaxNew,
    DeadlineReached,
    /// Added, but the budget was spent before its items could be wanted.
    BudgetExhausted,
    /// The item count was still rising when the poll ran out.
    Importing,
    /// The manager lists no items for the series yet.
    NothingListed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesRow {
    pub id: String,
    pub already_listed: bool,
    pub added: bool,
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub expected: Option<usize>,
    #[serde(default)]
    pub loaded: Option<usize>,
    /// False when the item list may still grow.
    pub settled: bool,
    /// Items the metadata source reports beyond what settled in the manager.
    #[serde(default)]
    pub shortfall: Option<usize>,
    #[serde(default)]
    pub wantable: Option<usize>,
    #[serde(default)]
    pub selected: Option<usize>,
    #[serde(default)]
    pub remaining: Option<usize>,
    #[serde(default)]
    pub actions: Vec<GapAction>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub pending: Option<Pending>,
}

impl SeriesRow {
    fn new(id: String, already_listed: bool) -> Self {
        SeriesRow {
            id,
            already_listed,
            added: false,
            series: None,
            expected: None,
            loaded: None,
            settled: true,
            shortfall: None,
            wantable: None,
            selected: None,
            remaining: None,
            actions: Vec::new(),
            error: None,
            pending: None,
        }
    }

    fn record(&mut self, view: &SeriesView, requested: Requested) {
        self.series = view.name.clone();
        self.expected = view.expected;
        self.loaded = Some(view.items.len());
        self.wantable = Some(requested.selected + requested.remaining);
        self.selected = Some(requested.selected);
        self.remaining = Some(requested.remaining);
        self.actions = requested.actions;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesAddReport {
    pub manager: String,
    pub requester: String,
    pub dry_run: bool,
    pub want: bool,
    pub added: usize,
    pub already_listed: usize,
    /// Want-requests issued without error.
    pub queued: usize,
    pub series_failed: usize,
    pub items_failed: usize,
    pub selected: usize,
    pub remaining: usize,
    /// Series whose wantable items were not read (not yet on the manager).
    pub unknown: usize,
    pub pending: usize,
    /// Settled series with a shortfall.
    pub short: usize,
    pub series: Vec<SeriesRow>,
}

impl SeriesAddReport {
    fn new(
        manager: &str,
        requester: &str,
        series: Vec<SeriesRow>,
        want: bool,
        dry_run: bool,
    ) -> Self {
        let sent = || series.iter().flat_map(|s| &s.actions).filter(|a| a.sent);
        SeriesAddReport {
            manager: manager.to_string(),
            requester: requester.to_string(),
            dry_run,
            want,
            added: series.iter().filter(|s| s.added).count(),
            already_listed: series.iter().filter(|s| s.already_listed).count(),
            queued: sent().filter(|a| a.error.is_none()).count(),
            series_failed: series.iter().filter(|s| s.error.is_some()).count(),
            items_failed: sent().filter(|a| a.error.is_some()).count(),
            selected: series.iter().filter_map(|s| s.selected).sum(),
            remaining: series.iter().filter_map(|s| s.remaining).sum(),
            unknown: series
                .iter()
                .filter(|s| want && s.wantable.is_none() && s.error.is_none())
                .count(),
            pending: series.iter().filter(|s| s.pending.is_some()).count(),
            short: series.iter().filter(|s| s.shortfall.is_some()).count(),
            series,
        }
    }
}

/// Add each id not yet on `manager` (at most `plan.max_new` per run) and, when
/// `plan.want`, want each series' missing items within `budget`. Stops starting
/// work once the budget's deadline is reached; per-series failures land in the
/// row. A dry run adds nothing and only sizes the wants of listed series.
pub async fn add_series(
    ids: &[String],
    manager: &dyn SeriesManager,
    requester: &dyn Requester,
    plan: &SeriesAddPlan,
    budget: &mut Budget,
    dry_run: bool,
) -> Result<SeriesAddReport, MediaError> {
    let on_list: BTreeSet<String> = manager.listed().await?.into_iter().collect();
    let mut rows = Vec::with_capacity(ids.len());
    let mut new = 0;
    for id in ids {
        let mut row = SeriesRow::new(id.clone(), on_list.contains(id));
        if dry_run {
            if plan.want && row.already_listed {
                match manager.series(id).await {
                    Ok(view) => {
                        let gaps = compute_gaps(&view.items, &view.owned);
                        let requested =
                            request_gaps(&gaps, requester, &plan.quality, budget, true).await;
                        row.record(&view, requested);
                    }
                    Err(e) => row.error = Some(e.to_string()),
                }
            }
        } else {
            if !row.already_listed && new >= plan.max_new {
                row.pending = Some(Pending::MaxNew);
            } else if budget.expired() {
                row.pending = Some(Pending::DeadlineReached);
            } else {
                add_and_want(manager, requester, plan, budget, &mut row).await;
            }
            let run_stopped = row.pending == Some(Pending::DeadlineReached) && budget.expired();
            if !row.already_listed && !run_stopped {
                new += 1;
            }
        }
        rows.push(row);
    }
    Ok(SeriesAddReport::new(
        manager.name(),
        requester.name(),
        rows,
        plan.want,
        dry_run,
    ))
}

async fn add_and_want(
    manager: &dyn SeriesManager,
    requester: &dyn Requester,
    plan: &SeriesAddPlan,
    budget: &mut Budget,
    row: &mut SeriesRow,
) {
    if !row.already_listed {
        if let Err(e) = manager.add(&row.id).await {
            row.error = Some(e.to_string());
            return;
        }
        match listed(
            manager,
            &row.id,
            plan.poll,
            &poll_deadline(budget, plan.poll),
        )
        .await
        {
            Ok(Polled::Settled(())) => row.added = true,
            Ok(Polled::Expired(())) => {
                row.pending = Some(Pending::DeadlineReached);
                return;
            }
            Ok(Polled::Exhausted(())) => {
                row.error = Some(format!(
                    "{} accepted the add but the series never appeared in its list",
                    manager.name()
                ));
                return;
            }
            Err(e) => {
                row.error = Some(e.to_string());
                return;
            }
        }
    }
    if !plan.want {
        return;
    }
    if row.added && budget.left() == 0 {
        row.pending = Some(Pending::BudgetExhausted);
        return;
    }
    let read = if row.already_listed {
        manager.series(&row.id).await.map(Polled::Settled)
    } else {
        imported(
            manager,
            &row.id,
            plan.poll,
            &poll_deadline(budget, plan.poll),
        )
        .await
    };
    let (view, settled) = match read {
        Ok(Polled::Settled(view)) => (view, true),
        Ok(Polled::Exhausted(view)) => (view, false),
        Ok(Polled::Expired(_)) => {
            row.pending = Some(Pending::DeadlineReached);
            return;
        }
        Err(e) => {
            row.error = Some(e.to_string());
            return;
        }
    };
    let gaps = compute_gaps(&view.items, &view.owned);
    let requested = request_gaps(&gaps, requester, &plan.quality, budget, false).await;
    let deadline_hit = requested
        .actions
        .iter()
        .any(|a| a.skipped == Some(SkipReason::DeadlineReached));
    row.settled = settled;
    row.record(&view, requested);
    let loaded = view.items.len();
    row.pending = if deadline_hit {
        Some(Pending::DeadlineReached)
    } else if loaded == 0 {
        Some(Pending::NothingListed)
    } else if !settled {
        Some(Pending::Importing)
    } else {
        row.shortfall = view.expected.filter(|&t| t > loaded).map(|t| t - loaded);
        None
    };
}

/// Without a run deadline each poll gets its own, so one slow wait never eats
/// into the next.
fn poll_deadline(budget: &Budget, poll: Poll) -> Deadline {
    budget
        .deadline()
        .copied()
        .unwrap_or_else(|| Deadline::after(poll.interval.saturating_mul(poll.attempts.max(1))))
}

async fn listed(
    manager: &dyn SeriesManager,
    id: &str,
    poll: Poll,
    deadline: &Deadline,
) -> Result<Polled<()>, MediaError> {
    poll_until(poll.interval, poll.attempts, deadline, |_| async move {
        let on_list = manager.listed().await?.iter().any(|s| s == id);
        Ok(if on_list {
            Probe::Settled(())
        } else {
            Probe::Pending(())
        })
    })
    .await
}

/// Settles once the series lists items and either reaches its expected count or
/// lists the same count on two consecutive probes.
async fn imported(
    manager: &dyn SeriesManager,
    id: &str,
    poll: Poll,
    deadline: &Deadline,
) -> Result<Polled<SeriesView>, MediaError> {
    let last = AtomicUsize::new(0);
    let last = &last;
    poll_until(poll.interval, poll.attempts, deadline, |_| async move {
        let view = manager.series(id).await?;
        let n = view.items.len();
        let complete = view.expected.is_some_and(|t| n >= t);
        let steady = last.swap(n, Ordering::Relaxed) == n;
        Ok(if n > 0 && (complete || steady) {
            Probe::Settled(view)
        } else {
            Probe::Pending(view)
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::{AddRequest, RequestState};
    use crate::search_driver::SearchPolicy;
    use crate::{ExternalId, MediaMutation, MediaType, ReleaseRef, RequestStatus, SearchHit};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    fn issue(id: &str) -> MediaIdentity {
        MediaIdentity {
            title: format!("#{id}"),
            year: None,
            external_ids: vec![ExternalId {
                source: "issue".into(),
                id: id.into(),
            }],
            series: None,
        }
    }

    fn view(ids: &[&str], owned: &[&str], expected: Option<usize>) -> SeriesView {
        SeriesView {
            name: Some("Detective Comics".into()),
            expected,
            items: ids.iter().map(|i| issue(i)).collect(),
            owned: owned.iter().map(|i| issue(i)).collect(),
        }
    }

    fn numbered(prefix: &str, n: usize, expected: Option<usize>) -> SeriesView {
        let ids: Vec<String> = (0..n).map(|i| format!("{prefix}{i}")).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        view(&ids, &[], expected)
    }

    #[derive(Default)]
    struct Mgr {
        lists: Mutex<VecDeque<Vec<String>>>,
        views: Mutex<BTreeMap<String, VecDeque<SeriesView>>>,
        refuse: Option<&'static str>,
        calls: Mutex<Vec<String>>,
    }

    impl Mgr {
        fn lists(self, lists: &[&[&str]]) -> Self {
            *self.lists.lock().unwrap() = lists
                .iter()
                .map(|l| l.iter().map(|s| s.to_string()).collect())
                .collect();
            self
        }

        fn views(self, id: &str, views: Vec<SeriesView>) -> Self {
            self.views.lock().unwrap().insert(id.into(), views.into());
            self
        }

        fn sent(&self, call: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| *c == call)
                .count()
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn next<T: Clone>(q: &mut VecDeque<T>) -> Option<T> {
        if q.len() > 1 {
            q.pop_front()
        } else {
            q.front().cloned()
        }
    }

    #[orca_async]
    impl SeriesManager for Mgr {
        fn name(&self) -> &str {
            "mylar3"
        }
        async fn listed(&self) -> Result<Vec<String>, MediaError> {
            self.calls.lock().unwrap().push("listed".into());
            Ok(next(&mut self.lists.lock().unwrap()).unwrap_or_default())
        }
        async fn add(&self, _id: &str) -> Result<(), MediaError> {
            self.calls.lock().unwrap().push("add".into());
            self.refuse
                .map_or(Ok(()), |why| Err(MediaError::Other(why.into())))
        }
        async fn series(&self, id: &str) -> Result<SeriesView, MediaError> {
            self.calls.lock().unwrap().push("series".into());
            let mut views = self.views.lock().unwrap();
            Ok(views.get_mut(id).and_then(next).unwrap_or_default())
        }
    }

    #[derive(Default)]
    struct Req {
        queued: Mutex<Vec<String>>,
        fail: Option<&'static str>,
    }

    impl Req {
        fn queued(&self) -> Vec<String> {
            self.queued.lock().unwrap().clone()
        }
    }

    fn status(identity: &MediaIdentity, state: RequestState) -> RequestStatus {
        RequestStatus {
            identity: identity.clone(),
            state,
            active_release: None,
            detail: None,
        }
    }

    #[orca_async]
    impl Requester for Req {
        fn name(&self) -> &str {
            "mylar3"
        }
        fn media_type(&self) -> MediaType {
            MediaType::Comics
        }
        async fn add(&self, req: &AddRequest) -> Result<RequestStatus, MediaError> {
            let id = req.identity.external_ids[0].id.clone();
            self.queued.lock().unwrap().push(id.clone());
            if self.fail == Some(id.as_str()) {
                return Err(MediaError::Transport("nope".into()));
            }
            Ok(status(&req.identity, RequestState::Wanted))
        }
        async fn search(&self, _id: &MediaIdentity) -> Result<Vec<SearchHit>, MediaError> {
            Ok(vec![])
        }
        async fn status(&self, id: &MediaIdentity) -> Result<RequestStatus, MediaError> {
            Ok(status(id, RequestState::Unknown))
        }
        async fn blocklist(
            &self,
            id: &MediaIdentity,
            _r: &ReleaseRef,
        ) -> Result<RequestStatus, MediaError> {
            Ok(status(id, RequestState::Searching))
        }
        async fn import_trigger(&self, _id: &MediaIdentity) -> Result<MediaMutation, MediaError> {
            Ok(MediaMutation {
                ok: true,
                message: None,
            })
        }
    }

    const FAST: Poll = Poll {
        interval: Duration::from_secs(5),
        attempts: 5,
    };

    fn plan(want: bool) -> SeriesAddPlan {
        SeriesAddPlan {
            want,
            max_new: 5,
            poll: FAST,
            quality: QualityPolicy::default(),
        }
    }

    fn budget(limit: usize) -> Budget {
        Budget::new(limit, &SearchPolicy::default()).unwrap()
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    async fn run(mgr: &Mgr, req: &Req, which: &[&str], plan: &SeriesAddPlan) -> SeriesAddReport {
        add_series(&ids(which), mgr, req, plan, &mut budget(100), false)
            .await
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn skips_listed_ids_and_wants_unowned_items() {
        let mgr = Mgr::default()
            .lists(&[&["1"], &["1", "2"]])
            .views("1", vec![view(&["10"], &["10"], None)])
            .views("2", vec![view(&["20", "21", "22"], &["21"], Some(3))]);
        let req = Req {
            fail: Some("22"),
            ..Req::default()
        };
        let r = run(&mgr, &req, &["1", "2"], &plan(true)).await;
        assert_eq!(
            (
                r.added,
                r.already_listed,
                r.queued,
                r.series_failed,
                r.items_failed
            ),
            (1, 1, 1, 0, 1)
        );
        assert!(r.series[0].already_listed && !r.series[0].added);
        assert_eq!(r.series[1].series.as_deref(), Some("Detective Comics"));
        assert_eq!(req.queued(), vec!["20", "22"]);
        assert_eq!(mgr.sent("add"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn without_want_queues_nothing() {
        let mgr = Mgr::default().lists(&[&[], &["2"]]);
        let req = Req::default();
        let r = run(&mgr, &req, &["2"], &plan(false)).await;
        assert_eq!(
            (r.added, r.queued, r.series_failed, r.items_failed),
            (1, 0, 0, 0)
        );
        assert_eq!(mgr.calls(), vec!["listed", "add", "listed"]);
    }

    #[tokio::test(start_paused = true)]
    async fn series_with_no_items_yet_is_pending() {
        let mgr = Mgr::default()
            .lists(&[&[], &["2"]])
            .views("2", vec![view(&[], &[], None)]);
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!((r.added, r.series_failed, r.pending), (1, 0, 1));
        assert_eq!(r.series[0].pending, Some(Pending::NothingListed));
    }

    #[tokio::test(start_paused = true)]
    async fn add_never_listed_is_a_row_error() {
        let mgr = Mgr::default().lists(&[&[]]);
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!((r.added, r.series_failed, r.items_failed), (0, 1, 0));
        let err = r.series[0].error.as_deref().unwrap();
        assert!(err.contains("never appeared in its list"), "{err}");
        assert_eq!(mgr.sent("series"), 0);
        assert_eq!(mgr.sent("listed"), 1 + FAST.attempts as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn rerun_wants_unowned_items_of_a_listed_series() {
        let mgr = Mgr::default()
            .lists(&[&["2"]])
            .views("2", vec![numbered("i", 3, Some(3))]);
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!(
            (
                r.added,
                r.already_listed,
                r.queued,
                r.series_failed,
                r.items_failed
            ),
            (0, 1, 3, 0, 0)
        );
        assert_eq!(mgr.sent("add"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn paces_between_wants_across_series_but_not_before_the_first() {
        let mgr = Mgr::default()
            .lists(&[&["2", "3"]])
            .views("2", vec![numbered("a", 2, Some(2))])
            .views("3", vec![numbered("b", 1, Some(1))]);
        let policy = SearchPolicy {
            request_pace_ms: 30_000,
            ..SearchPolicy::default()
        };
        let start = tokio::time::Instant::now();
        let r = add_series(
            &ids(&["2", "3"]),
            &mgr,
            &Req::default(),
            &plan(true),
            &mut Budget::new(100, &policy).unwrap(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(r.queued, 3);
        assert_eq!(start.elapsed(), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn without_want_reads_nothing_for_a_listed_series() {
        let mgr = Mgr::default().lists(&[&["2"]]);
        let r = run(&mgr, &Req::default(), &["2"], &plan(false)).await;
        assert_eq!((r.added, r.already_listed, r.queued), (0, 1, 0));
        assert_eq!(mgr.calls(), vec!["listed"]);
    }

    #[tokio::test(start_paused = true)]
    async fn add_refusal_is_a_row_error() {
        let mgr = Mgr {
            refuse: Some("bad id"),
            ..Mgr::default()
        };
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!((r.added, r.series_failed, r.items_failed), (0, 1, 0));
        assert!(r.series[0].error.as_deref().unwrap().contains("bad id"));
    }

    #[test]
    fn report_counts_series_and_item_failures_apart() {
        let action = |id: &str, error: Option<&str>| GapAction {
            identity: issue(id),
            sent: true,
            status: None,
            error: error.map(Into::into),
            skipped: None,
        };
        let mut bad = SeriesRow::new("1".into(), false);
        bad.error = Some("bad id".into());
        let mut good = SeriesRow::new("2".into(), false);
        good.actions = vec![
            action("a", Some("nope")),
            action("b", Some("nope")),
            action("c", None),
        ];
        let r = SeriesAddReport::new("m", "m", vec![bad, good], true, false);
        assert_eq!((r.series_failed, r.items_failed, r.queued), (1, 2, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_a_partial_import_to_complete() {
        let mgr = Mgr::default().lists(&[&[], &["2"]]).views(
            "2",
            vec![
                numbered("i", 0, None),
                numbered("i", 2, Some(5)),
                numbered("i", 4, Some(5)),
                numbered("i", 5, Some(5)),
            ],
        );
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!(
            (r.added, r.queued, r.series_failed, r.items_failed),
            (1, 5, 0, 0)
        );
        assert_eq!(mgr.sent("series"), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn settled_short_import_is_complete_with_a_shortfall() {
        let mgr = Mgr::default()
            .lists(&[&[], &["2"]])
            .views("2", vec![numbered("i", 3, Some(10))]);
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!(
            (r.added, r.queued, r.series_failed, r.pending, r.short),
            (1, 3, 0, 0, 1)
        );
        let s = &r.series[0];
        assert_eq!(
            (s.expected, s.loaded, s.shortfall, s.settled),
            (Some(10), Some(3), Some(7), true)
        );
        assert_eq!(mgr.sent("series"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn import_still_rising_when_the_poll_runs_out_is_pending() {
        let rising = (1..=FAST.attempts as usize)
            .map(|n| numbered("i", n, None))
            .collect();
        let mgr = Mgr::default().lists(&[&[], &["2"]]).views("2", rising);
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!(
            (r.series_failed, r.items_failed, r.pending, r.short),
            (0, 0, 1, 0)
        );
        let s = &r.series[0];
        assert_eq!(s.pending, Some(Pending::Importing));
        assert_eq!((s.loaded, s.settled, s.shortfall), (Some(5), false, None));
        assert_eq!(mgr.sent("series"), FAST.attempts as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn dry_run_reads_listed_series_only() {
        let mgr = Mgr::default()
            .lists(&[&["1"]])
            .views("1", vec![numbered("i", 3, Some(3))]);
        let req = Req::default();
        let r = add_series(
            &ids(&["1", "2"]),
            &mgr,
            &req,
            &plan(true),
            &mut budget(2),
            true,
        )
        .await
        .unwrap();
        assert!(r.dry_run);
        assert_eq!((r.added, r.already_listed), (0, 1));
        assert_eq!((r.selected, r.remaining, r.unknown), (2, 1, 1));
        assert_eq!(r.series[1].wantable, None);
        assert_eq!(mgr.calls(), vec!["listed", "series"]);
        assert!(req.queued().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn limit_spans_ids_and_reports_remaining() {
        let mgr = Mgr::default()
            .lists(&[&["1", "2", "3"]])
            .views("1", vec![numbered("a", 3, Some(3))])
            .views("2", vec![numbered("b", 3, Some(3))])
            .views("3", vec![numbered("c", 3, Some(3))]);
        let req = Req::default();
        let r = add_series(
            &ids(&["1", "2", "3"]),
            &mgr,
            &req,
            &plan(true),
            &mut budget(4),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            (
                r.queued,
                r.selected,
                r.remaining,
                r.series_failed,
                r.items_failed
            ),
            (4, 4, 5, 0, 0)
        );
        let left: Vec<_> = r.series.iter().map(|s| s.remaining).collect();
        assert_eq!(left, vec![Some(0), Some(2), Some(3)]);
        assert_eq!(req.queued().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_stops_polling_and_reports_the_rest_pending() {
        let mgr = Mgr::default().lists(&[&[]]);
        let r = add_series(
            &ids(&["2", "3"]),
            &mgr,
            &Req::default(),
            &plan(true),
            &mut budget(100).with_deadline(Duration::from_secs(7)),
            false,
        )
        .await
        .unwrap();
        assert_eq!((r.added, r.series_failed, r.pending), (0, 0, 2));
        assert!(
            r.series
                .iter()
                .all(|s| s.pending == Some(Pending::DeadlineReached))
        );
        assert_eq!(mgr.calls(), vec!["listed", "add", "listed", "listed"]);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_during_import_wait_defers_without_failing() {
        let mgr = Mgr::default()
            .lists(&[&[], &["2"]])
            .views("2", vec![numbered("i", 0, None)]);
        let r = add_series(
            &ids(&["2"]),
            &mgr,
            &Req::default(),
            &plan(true),
            &mut budget(100).with_deadline(Duration::from_secs(7)),
            false,
        )
        .await
        .unwrap();
        assert_eq!((r.added, r.series_failed, r.pending), (1, 0, 1));
        assert_eq!(r.series[0].pending, Some(Pending::DeadlineReached));
        assert_eq!(mgr.sent("series"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_budget_skips_the_import_wait() {
        let mgr = Mgr::default()
            .lists(&[&["1"], &["1", "2"]])
            .views("1", vec![numbered("a", 3, Some(3))]);
        let req = Req::default();
        let r = add_series(
            &ids(&["1", "2"]),
            &mgr,
            &req,
            &plan(true),
            &mut budget(3),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            (r.added, r.queued, r.series_failed, r.pending),
            (1, 3, 0, 1)
        );
        assert_eq!(r.series[1].pending, Some(Pending::BudgetExhausted));
        assert_eq!(mgr.sent("series"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn max_new_caps_attempted_adds() {
        let mgr = Mgr::default().lists(&[&[], &["2", "3"]]);
        let plan = SeriesAddPlan {
            max_new: 2,
            ..plan(false)
        };
        let r = run(&mgr, &Req::default(), &["2", "3", "4"], &plan).await;
        assert_eq!((r.added, r.pending, r.series_failed), (2, 1, 0));
        assert_eq!(r.series[2].pending, Some(Pending::MaxNew));
        assert_eq!(mgr.sent("add"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn listed_series_is_read_once() {
        let mgr = Mgr::default()
            .lists(&[&["2"]])
            .views("2", vec![numbered("i", 2, None)]);
        let plan = SeriesAddPlan {
            poll: Poll {
                interval: Duration::from_secs(3600),
                attempts: 5,
            },
            ..plan(true)
        };
        let start = tokio::time::Instant::now();
        let r = run(&mgr, &Req::default(), &["2"], &plan).await;
        assert_eq!((r.queued, r.series_failed), (2, 0));
        assert_eq!(mgr.calls(), vec!["listed", "series"]);
        assert!(start.elapsed() < Duration::from_secs(3600));
    }

    #[tokio::test(start_paused = true)]
    async fn settled_one_short_is_complete_with_shortfall_one() {
        let mgr = Mgr::default().lists(&[&[], &["2"]]).views(
            "2",
            vec![numbered("i", 9, Some(12)), numbered("i", 11, Some(12))],
        );
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!(
            (r.queued, r.series_failed, r.pending, r.short),
            (11, 0, 0, 1)
        );
        let s = &r.series[0];
        assert_eq!(
            (s.expected, s.loaded, s.shortfall, s.error.as_deref()),
            (Some(12), Some(11), Some(1), None)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn listed_series_short_read_is_complete_with_shortfall() {
        let mgr = Mgr::default()
            .lists(&[&["2"]])
            .views("2", vec![numbered("i", 11, Some(12))]);
        let r = run(&mgr, &Req::default(), &["2"], &plan(true)).await;
        assert_eq!(
            (r.queued, r.series_failed, r.pending, r.short),
            (11, 0, 0, 1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_listing_without_a_run_deadline_keeps_the_full_import_poll() {
        let mgr = Mgr::default()
            .lists(&[&[], &[], &[], &[], &["2"]])
            .views("2", (1..=5).map(|n| numbered("i", n, None)).collect());
        let plan = SeriesAddPlan {
            max_new: 1,
            ..plan(true)
        };
        let r = run(&mgr, &Req::default(), &["2", "3"], &plan).await;
        assert_eq!(r.series[0].pending, Some(Pending::Importing));
        assert_eq!(r.series[1].pending, Some(Pending::MaxNew));
        assert_eq!(mgr.sent("add"), 1);
    }
}
