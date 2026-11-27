use crate::seam::RequestState;
use crate::sequence::{GapPolicy, NumberRange, SeqNumber, SequenceGaps, SeriesSize, sequence_gaps};
use crate::status::rank;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::HashMap;
use thiserror::Error;

// Plain decimals only: f64 parsing also takes "nan", "inf" and "1e3".
pub fn decimal(s: &str) -> Option<f64> {
    let digits = |d: &str| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit());
    let unsigned = s.strip_prefix(['-', '+']).unwrap_or(s);
    let ok = match unsigned.split_once('.') {
        Some((whole, frac)) => digits(whole) && digits(frac),
        None => digits(unsigned),
    };
    ok.then(|| s.parse().ok()).flatten()
}

pub fn parse_number(s: &str) -> Option<SeqNumber> {
    let s = s.trim();
    if matches!(s, "½" | "1/2") {
        return Some(SeqNumber::Fraction(0.5));
    }
    if let Some(n) = decimal(s) {
        return Some(match s.parse::<i64>() {
            Ok(w) => SeqNumber::whole(w),
            Err(_) => SeqNumber::Fraction(n),
        });
    }
    let (a, b) = s.split_once(['-', '–'])?;
    let (from, to) = (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?);
    (from <= to).then(|| {
        SeqNumber::Whole(NumberRange {
            from: from.into(),
            to: to.into(),
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BoundError {
    #[error("{what} '{value}' is not a number")]
    NotANumber { what: &'static str, value: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Bounds {
    pub from: Option<f64>,
    pub to: Option<f64>,
}

impl Bounds {
    pub fn parse(from: Option<&str>, to: Option<&str>) -> Result<Self, BoundError> {
        let one = |v: Option<&str>, what| {
            v.map(|s| {
                decimal(s.trim()).ok_or_else(|| BoundError::NotANumber {
                    what,
                    value: s.to_string(),
                })
            })
            .transpose()
        };
        Ok(Self {
            from: one(from, "from")?,
            to: one(to, "to")?,
        })
    }

    pub fn ranged(&self) -> bool {
        self.from.is_some() || self.to.is_some()
    }

    fn span(n: SeqNumber) -> (f64, f64) {
        match n {
            SeqNumber::Whole(r) => (r.from as f64, r.to as f64),
            SeqNumber::Fraction(f) => (f, f),
        }
    }

    fn contains(&self, n: SeqNumber) -> bool {
        let (lo, hi) = Self::span(n);
        self.from.is_none_or(|f| lo >= f) && self.to.is_none_or(|t| hi <= t)
    }

    fn overlaps(&self, n: SeqNumber) -> bool {
        let (lo, hi) = Self::span(n);
        self.from.is_none_or(|f| hi >= f) && self.to.is_none_or(|t| lo <= t)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Include {
    pub in_flight: bool,
    pub skipped: bool,
}

fn in_flight(state: RequestState) -> bool {
    matches!(
        state,
        RequestState::Searching | RequestState::Snatched | RequestState::Downloading
    )
}

pub fn wantable(state: RequestState, include: Include) -> bool {
    match state {
        RequestState::Imported => false,
        RequestState::Skipped => include.skipped,
        s if in_flight(s) => include.in_flight,
        _ => true,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesEntry<S> {
    pub id: Option<String>,
    pub label: Option<String>,
    pub number: Option<SeqNumber>,
    pub status: S,
}

#[derive(Debug, PartialEq)]
pub struct Selection<'a, S> {
    pub picked: Vec<&'a SeriesEntry<S>>,
    /// Wantable entries a range cannot place: no number, or a range straddling a bound.
    pub unranged: Vec<&'a SeriesEntry<S>>,
}

pub fn select<'a, S>(
    entries: &'a [SeriesEntry<S>],
    bounds: Bounds,
    include: Include,
    state: impl Fn(&S) -> RequestState,
) -> Selection<'a, S> {
    let mut out = Selection {
        picked: Vec::new(),
        unranged: Vec::new(),
    };
    for e in entries
        .iter()
        .filter(|e| wantable(state(&e.status), include))
    {
        match e.number {
            _ if !bounds.ranged() => out.picked.push(e),
            Some(n) if bounds.contains(n) => out.picked.push(e),
            Some(n) if !bounds.overlaps(n) => {}
            _ => out.unranged.push(e),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusCount {
    pub state: RequestState,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AuditItem {
    pub id: Option<String>,
    pub label: Option<String>,
    pub state: RequestState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SeriesAudit {
    pub id: String,
    pub name: Option<String>,
    pub total: usize,
    pub by_status: Vec<StatusCount>,
    /// Wantable under the default [`Include`].
    pub missing: Vec<AuditItem>,
    pub in_flight: Vec<AuditItem>,
    pub skipped: Vec<AuditItem>,
    /// Missing entries without a backend id, which cannot be acted on.
    pub no_id: usize,
    pub unnumbered: Vec<AuditItem>,
    pub gaps: SequenceGaps,
    /// No missing entry, numbering gap or renumbered/sparse numbering.
    pub complete: bool,
}

fn tally(counts: impl Iterator<Item = (RequestState, usize)>) -> Vec<StatusCount> {
    let mut by: HashMap<RequestState, usize> = HashMap::new();
    for (state, n) in counts {
        *by.entry(state).or_default() += n;
    }
    let mut out: Vec<StatusCount> = by
        .into_iter()
        .map(|(state, count)| StatusCount { state, count })
        .collect();
    out.sort_by_key(|c| Reverse(rank(c.state)));
    out
}

impl SeriesAudit {
    /// `gaps: None` skips gap detection, for entries numbered apart from the run (annuals).
    pub fn build<S>(
        id: impl Into<String>,
        name: Option<String>,
        expected: Option<usize>,
        entries: &[SeriesEntry<S>],
        gaps: Option<&GapPolicy>,
        state: impl Fn(&S) -> RequestState,
    ) -> Self {
        let items: Vec<(AuditItem, Option<SeqNumber>)> = entries
            .iter()
            .map(|e| {
                let item = AuditItem {
                    id: e.id.clone(),
                    label: e.label.clone(),
                    state: state(&e.status),
                };
                (item, e.number)
            })
            .collect();
        let pick = |f: &dyn Fn(&AuditItem, Option<SeqNumber>) -> bool| -> Vec<AuditItem> {
            items
                .iter()
                .filter(|(i, n)| f(i, *n))
                .map(|(i, _)| i.clone())
                .collect()
        };
        let missing = pick(&|i, _| wantable(i.state, Include::default()));
        let numbers: Vec<SeqNumber> = items.iter().filter_map(|(_, n)| *n).collect();
        let gaps = gaps
            .map(|p| {
                let size = SeriesSize {
                    entries: entries.len(),
                    expected,
                };
                sequence_gaps(&numbers, size, p)
            })
            .unwrap_or_default();
        Self {
            id: id.into(),
            name,
            total: entries.len(),
            by_status: tally(items.iter().map(|(i, _)| (i.state, 1))),
            no_id: missing.iter().filter(|m| m.id.is_none()).count(),
            complete: missing.is_empty()
                && gaps.missing_count == 0
                && gaps.renumbered_or_sparse.is_none(),
            missing,
            in_flight: pick(&|i, _| in_flight(i.state)),
            skipped: pick(&|i, _| i.state == RequestState::Skipped),
            unnumbered: pick(&|_, n| n.is_none()),
            gaps,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AuditSummary {
    pub series: usize,
    pub entries: usize,
    pub incomplete: usize,
    pub missing: usize,
    pub in_flight: usize,
    pub skipped: usize,
    pub no_id: usize,
    pub unnumbered: usize,
    pub series_with_gaps: usize,
    /// Numbers absent across every gap range, including any truncated away.
    pub gap_numbers: usize,
    pub renumbered_or_sparse: usize,
    pub outliers: usize,
    pub by_status: Vec<StatusCount>,
}

impl AuditSummary {
    pub fn of<'a>(series: impl IntoIterator<Item = &'a SeriesAudit>) -> Self {
        let mut out = Self::default();
        let mut by = Vec::new();
        for s in series {
            out.series += 1;
            out.entries += s.total;
            out.incomplete += usize::from(!s.complete);
            out.missing += s.missing.len();
            out.in_flight += s.in_flight.len();
            out.skipped += s.skipped.len();
            out.no_id += s.no_id;
            out.unnumbered += s.unnumbered.len();
            out.series_with_gaps += usize::from(s.gaps.missing_count > 0);
            out.gap_numbers += s.gaps.missing_count;
            out.renumbered_or_sparse += usize::from(s.gaps.renumbered_or_sparse.is_some());
            out.outliers += s.gaps.outliers.len();
            by.extend(s.by_status.iter().map(|c| (c.state, c.count)));
        }
        out.by_status = tally(by.into_iter());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequence::{GapStart, SparsePolicy};

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Mylar {
        Downloaded,
        Archived,
        Wanted,
        Skipped,
        Failed,
        Snatched,
        Ignored,
    }

    fn mylar(s: &Mylar) -> RequestState {
        match s {
            Mylar::Downloaded | Mylar::Archived => RequestState::Imported,
            Mylar::Wanted => RequestState::Wanted,
            Mylar::Skipped => RequestState::Unmonitored,
            Mylar::Failed => RequestState::Failed,
            Mylar::Snatched => RequestState::Snatched,
            Mylar::Ignored => RequestState::Skipped,
        }
    }

    fn issue(id: &str, number: &str, status: Mylar) -> SeriesEntry<Mylar> {
        SeriesEntry {
            id: Some(id.into()),
            label: Some(number.into()),
            number: parse_number(number),
            status,
        }
    }

    fn issues_policy() -> GapPolicy {
        GapPolicy {
            sparse: Some(SparsePolicy::default()),
            max_ranges: Some(50),
            ..GapPolicy::default()
        }
    }

    fn r(from: i64, to: i64) -> NumberRange {
        NumberRange { from, to }
    }

    fn ids<S>(es: &[&SeriesEntry<S>]) -> Vec<String> {
        es.iter().filter_map(|e| e.id.clone()).collect()
    }

    fn saga() -> Vec<SeriesEntry<Mylar>> {
        vec![
            issue("1", "1", Mylar::Downloaded),
            issue("2", "2", Mylar::Skipped),
            issue("4", "4", Mylar::Failed),
            issue("5", "5", Mylar::Archived),
            issue("6", "6", Mylar::Snatched),
            issue("7", "7", Mylar::Ignored),
        ]
    }

    #[test]
    fn decimal_is_plain_digits_only() {
        assert_eq!(decimal("12"), Some(12.0));
        assert_eq!(decimal("-1.5"), Some(-1.5));
        assert_eq!(decimal("+1"), Some(1.0));
        for bad in ["", "nan", "inf", "1e3", "1.", ".5", "AU", "+-1", "--1"] {
            assert_eq!(decimal(bad), None, "{bad}");
        }
    }

    #[test]
    fn parse_number_reads_whole_fraction_and_range() {
        assert_eq!(parse_number(" 006 "), Some(SeqNumber::whole(6)));
        assert_eq!(parse_number("3.5"), Some(SeqNumber::Fraction(3.5)));
        assert_eq!(parse_number("½"), Some(SeqNumber::Fraction(0.5)));
        assert_eq!(parse_number("1/2"), Some(SeqNumber::Fraction(0.5)));
        assert_eq!(parse_number("7–9"), Some(SeqNumber::Whole(r(7, 9))));
        assert_eq!(parse_number("-1"), Some(SeqNumber::whole(-1)));
        assert_eq!(parse_number("+1"), Some(SeqNumber::whole(1)));
        assert_eq!(parse_number("3-1"), None);
        assert_eq!(parse_number("AU"), None);
    }

    #[test]
    fn bounds_parse_rejects_non_numbers() {
        assert_eq!(
            Bounds::parse(Some(" 2 "), Some("4.5")).unwrap(),
            Bounds {
                from: Some(2.0),
                to: Some(4.5)
            }
        );
        assert_eq!(
            Bounds::parse(Some("one"), None),
            Err(BoundError::NotANumber {
                what: "from",
                value: "one".into()
            })
        );
        assert!(!Bounds::parse(None, None).unwrap().ranged());
    }

    #[test]
    fn select_defaults_to_missing_only() {
        let e = saga();
        let s = select(&e, Bounds::default(), Include::default(), mylar);
        assert_eq!(ids(&s.picked), vec!["2", "4"]);
        assert!(s.unranged.is_empty());
    }

    #[test]
    fn select_opts_in_to_in_flight_and_skipped() {
        let e = saga();
        for (include, want) in [
            (
                Include {
                    in_flight: true,
                    skipped: false,
                },
                vec!["2", "4", "6"],
            ),
            (
                Include {
                    in_flight: false,
                    skipped: true,
                },
                vec!["2", "4", "7"],
            ),
            (
                Include {
                    in_flight: true,
                    skipped: true,
                },
                vec!["2", "4", "6", "7"],
            ),
        ] {
            assert_eq!(
                ids(&select(&e, Bounds::default(), include, mylar).picked),
                want
            );
        }
    }

    #[test]
    fn select_range_is_inclusive_and_reports_unranged() {
        let mut e = saga();
        e.push(issue("8", "AU", Mylar::Wanted));
        e.push(issue("9", "4.5", Mylar::Wanted));
        e.push(issue("10", "4-5", Mylar::Wanted));
        e.push(issue("11", "8-9", Mylar::Wanted));
        e.push(issue("12", "3-4", Mylar::Wanted));
        let b = Bounds::parse(Some("3"), Some("4.5")).unwrap();
        let s = select(&e, b, Include::default(), mylar);
        assert_eq!(ids(&s.picked), vec!["4", "9", "12"]);
        assert_eq!(ids(&s.unranged), vec!["8", "10"]);
        let open = Bounds::parse(None, Some("2")).unwrap();
        assert_eq!(
            ids(&select(&e, open, Include::default(), mylar).picked),
            vec!["2"]
        );
    }

    #[test]
    fn audit_counts_statuses_and_lists_missing() {
        let a = SeriesAudit::build(
            "100",
            Some("Saga".into()),
            None,
            &[
                issue("1", "1", Mylar::Downloaded),
                issue("2", "2", Mylar::Skipped),
                issue("3", "3", Mylar::Archived),
                issue("4", "4", Mylar::Wanted),
                issue("5", "5", Mylar::Ignored),
                issue("6", "6", Mylar::Snatched),
            ],
            Some(&issues_policy()),
            mylar,
        );
        assert_eq!(a.total, 6);
        assert!(!a.complete);
        assert_eq!(
            a.missing
                .iter()
                .map(|m| (m.label.as_deref().unwrap(), m.state))
                .collect::<Vec<_>>(),
            vec![
                ("2", RequestState::Unmonitored),
                ("4", RequestState::Wanted)
            ]
        );
        assert_eq!(a.in_flight[0].id.as_deref(), Some("6"));
        assert_eq!(a.skipped[0].id.as_deref(), Some("5"));
        assert_eq!(
            a.by_status
                .iter()
                .find(|c| c.state == RequestState::Imported)
                .unwrap()
                .count,
            2
        );
        let held = SeriesAudit::build(
            "1",
            None,
            None,
            &[
                issue("1", "1", Mylar::Downloaded),
                issue("2", "2", Mylar::Snatched),
                issue("3", "3", Mylar::Ignored),
            ],
            Some(&issues_policy()),
            mylar,
        );
        assert!(held.complete);
        assert!(held.missing.is_empty());
    }

    #[test]
    fn audit_gaps_and_no_id() {
        let mut e = saga();
        e.push(SeriesEntry {
            id: None,
            label: Some("8".into()),
            number: Some(SeqNumber::whole(8)),
            status: Mylar::Wanted,
        });
        e.push(issue("x", "AU", Mylar::Downloaded));
        let a = SeriesAudit::build("100", None, None, &e, Some(&issues_policy()), mylar);
        assert_eq!(a.gaps.missing, vec![r(3, 3)]);
        assert_eq!(a.no_id, 1);
        assert_eq!(a.unnumbered.len(), 1);
        let annuals = SeriesAudit::build("100", None, None, &e, None, mylar);
        assert_eq!(annuals.gaps, SequenceGaps::default());
    }

    #[test]
    fn sparse_series_is_incomplete_and_counted() {
        let e = [
            issue("1", "1", Mylar::Downloaded),
            issue("2", "700", Mylar::Downloaded),
        ];
        let a = SeriesAudit::build("c", None, None, &e, Some(&issues_policy()), mylar);
        assert!(a.gaps.renumbered_or_sparse.is_some());
        assert!(!a.complete);
        let s = AuditSummary::of([&a]);
        assert_eq!((s.incomplete, s.renumbered_or_sparse), (1, 1));
    }

    #[test]
    fn summary_totals_every_series() {
        let saga = SeriesAudit::build("100", None, None, &saga(), Some(&issues_policy()), mylar);
        let done = SeriesAudit::build(
            "200",
            None,
            None,
            &[issue("21", "1", Mylar::Downloaded)],
            Some(&issues_policy()),
            mylar,
        );
        let s = AuditSummary::of([&saga, &done]);
        assert_eq!((s.series, s.entries, s.incomplete), (2, 7, 1));
        assert_eq!((s.missing, s.in_flight, s.skipped), (2, 1, 1));
        assert_eq!((s.series_with_gaps, s.gap_numbers), (1, 1));
        assert_eq!(
            s.by_status,
            vec![
                StatusCount {
                    state: RequestState::Imported,
                    count: 3
                },
                StatusCount {
                    state: RequestState::Snatched,
                    count: 1
                },
                StatusCount {
                    state: RequestState::Failed,
                    count: 1
                },
                StatusCount {
                    state: RequestState::Skipped,
                    count: 1
                },
                StatusCount {
                    state: RequestState::Unmonitored,
                    count: 1
                },
            ]
        );
    }

    fn book(seq: Option<&str>) -> SeriesEntry<()> {
        SeriesEntry {
            id: None,
            label: seq.map(String::from),
            number: seq.and_then(parse_number),
            status: (),
        }
    }

    #[test]
    fn library_server_entries_are_imported_and_gaps_summarize() {
        let books = GapPolicy {
            below: Some(1000),
            start: GapStart::FromOneWithin(5),
            ..GapPolicy::default()
        };
        let present = |_: &()| RequestState::Imported;
        let a = SeriesAudit::build(
            "s1",
            None,
            None,
            &[
                book(Some("2")),
                book(Some("5")),
                book(Some("1.5")),
                book(Some("7–9")),
                book(None),
            ],
            Some(&books),
            present,
        );
        assert_eq!(a.gaps.missing, vec![r(1, 1), r(3, 4), r(6, 6)]);
        assert!(a.missing.is_empty());
        assert!(!a.complete);
        let b = SeriesAudit::build("s2", None, None, &[book(Some("1"))], Some(&books), present);
        let s = AuditSummary::of([&a, &b]);
        assert_eq!(
            (
                s.series,
                s.entries,
                s.series_with_gaps,
                s.gap_numbers,
                s.unnumbered
            ),
            (2, 6, 1, 4, 1)
        );
    }
}
