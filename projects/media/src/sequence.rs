use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumberRange {
    pub from: i64,
    pub to: i64,
}

impl NumberRange {
    pub fn single(n: i64) -> Self {
        Self { from: n, to: n }
    }

    fn len(&self) -> usize {
        usize::try_from(self.to.abs_diff(self.from))
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SeqNumber {
    Whole(NumberRange),
    Fraction(f64),
}

impl SeqNumber {
    pub fn whole(n: i64) -> Self {
        Self::Whole(NumberRange::single(n))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesSize {
    pub entries: usize,
    pub expected: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fractional {
    #[default]
    Ignore,
    Truncate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GapStart {
    #[default]
    Lowest,
    FromOneWithin(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparsePolicy {
    pub slack: i64,
    pub small_series: usize,
    pub small_series_span: i64,
    pub max_outliers: usize,
}

impl Default for SparsePolicy {
    fn default() -> Self {
        Self {
            slack: 10,
            small_series: 10,
            small_series_span: 100,
            max_outliers: 3,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapPolicy {
    pub min_entries: usize,
    pub below: Option<i64>,
    pub fractional: Fractional,
    pub start: GapStart,
    pub sparse: Option<SparsePolicy>,
    pub max_ranges: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceGaps {
    pub missing: Vec<NumberRange>,
    pub missing_count: usize,
    pub outliers: Vec<i64>,
    pub renumbered_or_sparse: Option<NumberRange>,
    pub truncated: bool,
}

fn covered(numbers: &[SeqNumber], policy: &GapPolicy) -> Vec<NumberRange> {
    let mut ranges: Vec<NumberRange> = numbers
        .iter()
        .filter_map(|n| match *n {
            SeqNumber::Whole(r) => Some(r),
            SeqNumber::Fraction(f) if f.fract() == 0.0 && f.abs() < i64::MAX as f64 => {
                Some(NumberRange::single(f as i64))
            }
            SeqNumber::Fraction(f) => match policy.fractional {
                Fractional::Ignore => None,
                Fractional::Truncate => {
                    (f.is_finite() && f >= 0.0 && policy.below.is_none_or(|b| f < b as f64))
                        .then(|| NumberRange::single(f as i64))
                }
            },
        })
        .filter(|r| r.from >= 0 && r.from <= r.to && policy.below.is_none_or(|b| r.to < b))
        .collect();
    ranges.sort_by_key(|r| r.from);
    let mut merged: Vec<NumberRange> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(m) if r.from <= m.to.saturating_add(1) => m.to = m.to.max(r.to),
            _ => merged.push(r),
        }
    }
    merged
}

fn total(ranges: &[NumberRange]) -> usize {
    ranges.iter().fold(0, |n, r| n.saturating_add(r.len()))
}

pub fn sequence_gaps(numbers: &[SeqNumber], size: SeriesSize, policy: &GapPolicy) -> SequenceGaps {
    if size.entries < policy.min_entries {
        return SequenceGaps::default();
    }
    let mut ranges = covered(numbers, policy);
    let (Some(lo), Some(hi)) = (ranges.first().map(|r| r.from), ranges.last().map(|r| r.to)) else {
        return SequenceGaps::default();
    };
    let mut out = SequenceGaps::default();
    if let Some(sparse) = &policy.sparse {
        let distinct = total(&ranges);
        let count =
            i64::try_from(size.expected.unwrap_or(size.entries).max(distinct)).unwrap_or(i64::MAX);
        let limit = count.saturating_mul(2).saturating_add(sparse.slack);
        if hi.saturating_sub(lo).saturating_add(1) > limit {
            let small = size.expected.is_none()
                && distinct < sparse.small_series
                && hi.saturating_sub(lo) < sparse.small_series_span;
            let cut = lo.saturating_add(limit);
            let past: Vec<NumberRange> = ranges
                .iter()
                .filter(|r| r.to >= cut)
                .map(|r| NumberRange {
                    from: r.from.max(cut),
                    to: r.to,
                })
                .collect();
            let past_count = total(&past);
            if size.expected.is_some() && past_count > 0 && past_count <= sparse.max_outliers {
                out.outliers = past.iter().flat_map(|r| r.from..=r.to).collect();
                ranges.retain(|r| r.from < cut);
                if let Some(last) = ranges.last_mut() {
                    last.to = last.to.min(cut - 1);
                }
            } else if !small {
                out.renumbered_or_sparse = Some(NumberRange { from: lo, to: hi });
                return out;
            }
        }
    }
    let start = match policy.start {
        GapStart::FromOneWithin(at) if lo <= at => lo.min(1),
        _ => lo,
    };
    let mut prev = start.saturating_sub(1);
    for r in ranges {
        if r.from > prev.saturating_add(1) {
            let gap = NumberRange {
                from: prev + 1,
                to: r.from - 1,
            };
            out.missing_count = out.missing_count.saturating_add(gap.len());
            if policy.max_ranges == Some(out.missing.len()) {
                out.truncated = true;
            } else {
                out.missing.push(gap);
            }
        }
        prev = prev.max(r.to);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(from: i64, to: i64) -> NumberRange {
        NumberRange { from, to }
    }

    fn issues() -> GapPolicy {
        GapPolicy {
            sparse: Some(SparsePolicy::default()),
            max_ranges: Some(50),
            ..GapPolicy::default()
        }
    }

    fn books() -> GapPolicy {
        GapPolicy {
            below: Some(1000),
            start: GapStart::FromOneWithin(5),
            ..GapPolicy::default()
        }
    }

    fn comics() -> GapPolicy {
        GapPolicy {
            min_entries: 3,
            fractional: Fractional::Truncate,
            ..books()
        }
    }

    fn whole(ns: impl IntoIterator<Item = i64>) -> Vec<SeqNumber> {
        ns.into_iter().map(SeqNumber::whole).collect()
    }

    fn size(entries: usize, expected: Option<usize>) -> SeriesSize {
        SeriesSize { entries, expected }
    }

    fn issue_gaps(ns: &[SeqNumber], expected: Option<usize>) -> SequenceGaps {
        sequence_gaps(ns, size(ns.len(), expected), &issues())
    }

    fn book_gaps(ns: &[SeqNumber]) -> Vec<NumberRange> {
        sequence_gaps(ns, size(ns.len(), None), &books()).missing
    }

    fn comic_gaps(ns: &[f64]) -> Vec<NumberRange> {
        let ns: Vec<SeqNumber> = ns.iter().map(|f| SeqNumber::Fraction(*f)).collect();
        sequence_gaps(&ns, size(ns.len(), None), &comics()).missing
    }

    #[test]
    fn issues_gaps_are_missing_whole_numbers_between_the_ends() {
        let mut ns = whole([1, 3, 6]);
        ns.push(SeqNumber::Fraction(3.5));
        let g = sequence_gaps(&ns, size(5, None), &issues());
        assert_eq!(g.missing, vec![r(2, 2), r(4, 5)]);
        assert_eq!(
            (g.missing_count, g.truncated, g.renumbered_or_sparse),
            (3, false, None)
        );
        assert_eq!(
            sequence_gaps(&[], size(1, None), &issues()),
            SequenceGaps::default()
        );
    }

    #[test]
    fn issues_legacy_renumbering_is_sparse_not_hundreds_of_gaps() {
        let g = issue_gaps(&whole([1, 700]), None);
        assert!(g.missing.is_empty());
        assert_eq!(g.missing_count, 0);
        assert_eq!(g.renumbered_or_sparse, Some(r(1, 700)));
    }

    #[test]
    fn issues_outlier_is_split_out_and_dense_gaps_still_listed() {
        let mut ns = whole((1..=20).filter(|n| *n != 7));
        ns.push(SeqNumber::whole(2019));
        let g = issue_gaps(&ns, Some(20));
        assert_eq!(g.renumbered_or_sparse, None);
        assert_eq!(g.missing, vec![r(7, 7)]);
        assert_eq!((g.missing_count, g.outliers), (1, vec![2019]));
        ns.push(SeqNumber::whole(1_000_000));
        assert!(issue_gaps(&ns, None).renumbered_or_sparse.is_some());
    }

    #[test]
    fn issues_more_outliers_than_allowed_with_a_total_is_sparse() {
        let mut ns = whole(1..=20);
        ns.extend(whole([2019, 2020, 2021, 2022]));
        let g = issue_gaps(&ns, Some(20));
        assert_eq!(g.renumbered_or_sparse, Some(r(1, 2022)));
        assert!(g.outliers.is_empty());
    }

    #[test]
    fn issues_small_new_series_lists_gaps() {
        let g = issue_gaps(&whole([1, 15]), None);
        assert_eq!(g.renumbered_or_sparse, None);
        assert_eq!(g.missing, vec![r(2, 14)]);
        assert_eq!(g.missing_count, 13);
    }

    #[test]
    fn issues_negative_numbers_are_ignored() {
        let g = issue_gaps(&whole([1, 2, 4, -1]), None);
        assert_eq!(g.missing, vec![r(3, 3)]);
        assert_eq!(g.missing_count, 1);
    }

    #[test]
    fn issues_expected_total_widens_the_sparse_threshold() {
        let ns = whole([1, 150]);
        assert!(issue_gaps(&ns, None).renumbered_or_sparse.is_some());
        let g = issue_gaps(&ns, Some(150));
        assert_eq!(g.renumbered_or_sparse, None);
        assert_eq!(g.missing, vec![r(2, 149)]);
        assert_eq!(g.missing_count, 148);
    }

    #[test]
    fn issues_ranges_are_capped_and_flag_truncation() {
        let n = 55;
        let g = issue_gaps(&whole((0..=n).map(|i| i * 2)), None);
        assert_eq!(g.missing.len(), 50);
        assert!(g.truncated);
        assert_eq!(g.missing_count, n as usize);
        assert_eq!(g.missing[0], r(1, 1));
    }

    #[test]
    fn books_report_missing_runs_from_one() {
        assert_eq!(
            book_gaps(&whole([2, 5, 6, 9])),
            vec![r(1, 1), r(3, 4), r(7, 8)]
        );
        assert!(book_gaps(&whole([3, 1, 2])).is_empty());
        assert!(book_gaps(&whole([1, 1, 2, 2, 2])).is_empty());
    }

    #[test]
    fn books_decimals_are_novellas_not_gaps() {
        let f = SeqNumber::Fraction;
        let ns = [
            SeqNumber::whole(1),
            f(1.5),
            SeqNumber::whole(2),
            f(2.5),
            SeqNumber::whole(4),
        ];
        assert_eq!(book_gaps(&ns), vec![r(3, 3)]);
        assert!(book_gaps(&[f(0.5), f(1.5)]).is_empty());
    }

    #[test]
    fn books_omnibus_ranges_cover_their_span_only_when_wholly_below() {
        let ns = [
            SeqNumber::Whole(r(1, 3)),
            SeqNumber::whole(4),
            SeqNumber::whole(6),
            SeqNumber::Whole(r(7, 9)),
        ];
        assert_eq!(book_gaps(&ns), vec![r(5, 5)]);
        let ns = [SeqNumber::whole(1), SeqNumber::Whole(r(998, 1001))];
        assert!(book_gaps(&ns).is_empty());
        let ns = [SeqNumber::whole(1), SeqNumber::Whole(r(3, i64::MAX))];
        assert!(book_gaps(&ns).is_empty());
    }

    #[test]
    fn books_gaps_start_at_lowest_when_above_five() {
        assert_eq!(book_gaps(&whole([6, 8])), vec![r(7, 7)]);
        assert_eq!(book_gaps(&whole([5, 6])), vec![r(1, 4)]);
        assert!(book_gaps(&whole([0, 1])).is_empty());
        assert_eq!(book_gaps(&whole([0, 3])), vec![r(1, 2)]);
    }

    #[test]
    fn books_numbers_from_one_thousand_are_ignored() {
        assert_eq!(book_gaps(&whole([1, 3, 1000])), vec![r(2, 2)]);
        assert_eq!(book_gaps(&whole([1, 999])), vec![r(2, 998)]);
        assert!(book_gaps(&whole([1, 2019])).is_empty());
    }

    #[test]
    fn books_missing_count_sums_ranges() {
        let g = sequence_gaps(&whole([1, 4]), size(2, None), &books());
        assert_eq!(g.missing_count, 2);
    }

    #[test]
    fn huge_numbers_saturate_instead_of_overflowing() {
        let ns = whole([0, i64::MAX]);
        let g = sequence_gaps(&ns, size(2, None), &GapPolicy::default());
        assert_eq!(g.missing, vec![r(1, i64::MAX - 1)]);
        assert_eq!(g.missing_count, (i64::MAX - 1) as usize);
        let g = sequence_gaps(&ns, size(2, Some(u32::MAX as usize)), &issues());
        assert_eq!(g.outliers, vec![i64::MAX]);
    }

    #[test]
    fn comics_truncate_fractions_and_fill_from_one() {
        assert_eq!(comic_gaps(&[3.0, 4.0, 7.0, 7.0]), vec![r(1, 2), r(5, 6)]);
        assert_eq!(comic_gaps(&[1.0, 2.5, 4.0]), vec![r(3, 3)]);
    }

    #[test]
    fn comics_need_min_entries() {
        assert!(comic_gaps(&[1.0, 4.0]).is_empty());
        let ns = [SeqNumber::Fraction(1.0), SeqNumber::Fraction(4.0)];
        assert_eq!(
            sequence_gaps(&ns, size(3, None), &comics()).missing,
            vec![r(2, 3)]
        );
    }

    #[test]
    fn non_finite_and_negative_fractions_are_dropped() {
        let ns = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5, 1.0, 3.0];
        assert_eq!(comic_gaps(&ns), vec![r(2, 2)]);
        let open = GapPolicy {
            fractional: Fractional::Truncate,
            ..GapPolicy::default()
        };
        let ns = [SeqNumber::Fraction(f64::INFINITY), SeqNumber::whole(1)];
        assert!(sequence_gaps(&ns, size(2, None), &open).missing.is_empty());
    }

    #[test]
    fn truncate_keeps_values_just_below_the_limit() {
        assert_eq!(
            comic_gaps(&[1.0, 2.0, 997.0, 999.9]),
            vec![r(3, 996), r(998, 998)]
        );
        assert_eq!(comic_gaps(&[1.0, 2.0, 3.0, 1000.0]), vec![]);
    }

    mod parity {
        use super::super::*;
        use std::collections::{BTreeMap, BTreeSet};

        mod mylar {
            use std::collections::BTreeSet;

            const MAX_GAP_RANGES: usize = 50;
            const SPARSE_SLACK: i64 = 10;
            const SMALL_SERIES: usize = 10;
            const SMALL_SERIES_SPAN: i64 = 100;
            const MAX_OUTLIERS: usize = 3;

            pub struct IssueState {
                pub number: Option<String>,
            }

            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct NumberRange {
                pub from: i64,
                pub to: i64,
            }

            #[derive(Debug, Clone, Default, PartialEq, Eq)]
            pub struct Gaps {
                pub ranges: Vec<NumberRange>,
                pub missing: usize,
                pub truncated: bool,
                pub renumbered_or_sparse: Option<NumberRange>,
                pub outliers: Vec<i64>,
            }

            pub fn gaps(issues: &[IssueState], total_issues: Option<usize>) -> Gaps {
                let whole: BTreeSet<i64> = issues
                    .iter()
                    .filter_map(|i| i.number.as_deref()?.trim().parse().ok())
                    .filter(|n| *n >= 0)
                    .collect();
                let (Some(&lo), Some(&hi)) = (whole.first(), whole.last()) else {
                    return Gaps::default();
                };
                let count = total_issues.unwrap_or(issues.len()).max(whole.len()) as i64;
                let limit = 2 * count + SPARSE_SLACK;
                let mut outliers = Vec::new();
                let mut whole = whole;
                if hi - lo + 1 > limit {
                    let small = total_issues.is_none()
                        && whole.len() < SMALL_SERIES
                        && hi - lo < SMALL_SERIES_SPAN;
                    let past: Vec<i64> = match total_issues {
                        Some(_) => whole.range(lo + limit..).copied().collect(),
                        None => Vec::new(),
                    };
                    if !past.is_empty() && past.len() <= MAX_OUTLIERS {
                        whole.retain(|n| *n < lo + limit);
                        outliers = past;
                    } else if !small {
                        return Gaps {
                            renumbered_or_sparse: Some(NumberRange { from: lo, to: hi }),
                            ..Gaps::default()
                        };
                    }
                }
                let mut out = Gaps {
                    outliers,
                    ..Gaps::default()
                };
                for (&a, &b) in whole.iter().zip(whole.iter().skip(1)) {
                    if b - a < 2 {
                        continue;
                    }
                    out.missing += (b - a - 1) as usize;
                    if out.ranges.len() == MAX_GAP_RANGES {
                        out.truncated = true;
                    } else {
                        out.ranges.push(NumberRange {
                            from: a + 1,
                            to: b - 1,
                        });
                    }
                }
                out
            }
        }

        const GAP_MAX_NUMBER: i64 = 1000;

        #[derive(Debug, Clone, PartialEq)]
        enum Seq {
            Number(f64),
            Range(i64, i64),
            Other(String),
        }

        #[allow(clippy::collapsible_if)]
        fn parse_seq(s: &str) -> Seq {
            let s = s.trim();
            if let Ok(n) = s.parse::<f64>() {
                if n.is_finite() {
                    return Seq::Number(n);
                }
            }
            if let Some((a, b)) = s.split_once(['-', '–']) {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<i64>(), b.trim().parse::<i64>()) {
                    if a <= b {
                        return Seq::Range(a, b);
                    }
                }
            }
            Seq::Other(s.to_lowercase())
        }

        fn abs_gaps(seqs: &[&str]) -> Vec<NumberRange> {
            let mut covered: BTreeSet<i64> = BTreeSet::new();
            for seq in seqs.iter().map(|s| parse_seq(s)) {
                match seq {
                    Seq::Number(v) if v.fract() == 0.0 && v >= 0.0 && v < GAP_MAX_NUMBER as f64 => {
                        covered.insert(v as i64);
                    }
                    Seq::Range(a, b) if a >= 0 && b < GAP_MAX_NUMBER => {
                        covered.extend(a..=b);
                    }
                    _ => {}
                }
            }
            let mut gaps: Vec<NumberRange> = Vec::new();
            if let (Some(&first), Some(&last)) = (covered.first(), covered.last()) {
                let lo = if first <= 5 { 1 } else { first };
                for n in (lo..=last).filter(|n| !covered.contains(n)) {
                    match gaps.last_mut() {
                        Some(r) if r.to == n - 1 => r.to = n,
                        _ => gaps.push(NumberRange { from: n, to: n }),
                    }
                }
            }
            gaps
        }

        const GAP_MIN_BOOKS: usize = 3;
        const KOMGA_GAP_MAX_NUMBER: f64 = 1000.0;

        fn komga_gaps(entries: &[f64]) -> Vec<NumberRange> {
            if entries.len() < GAP_MIN_BOOKS {
                return Vec::new();
            }
            let nums: BTreeSet<i64> = entries
                .iter()
                .filter(|n| **n >= 0.0 && **n < KOMGA_GAP_MAX_NUMBER)
                .map(|n| *n as i64)
                .collect();
            let (Some(&first), Some(&last)) = (nums.first(), nums.last()) else {
                return Vec::new();
            };
            let lo = if first <= 5 { 1 } else { first };
            let mut out: Vec<NumberRange> = Vec::new();
            for n in (lo..=last).filter(|n| !nums.contains(n)) {
                match out.last_mut() {
                    Some(r) if r.to == n - 1 => r.to = n,
                    _ => out.push(NumberRange { from: n, to: n }),
                }
            }
            out
        }

        fn abs_input(seqs: &[&str]) -> Vec<SeqNumber> {
            seqs.iter()
                .filter_map(|s| match parse_seq(s) {
                    Seq::Number(v) => Some(SeqNumber::Fraction(v)),
                    Seq::Range(a, b) => Some(SeqNumber::Whole(NumberRange { from: a, to: b })),
                    Seq::Other(_) => None,
                })
                .collect()
        }

        fn policies() -> BTreeMap<&'static str, GapPolicy> {
            BTreeMap::from([
                (
                    "issues",
                    GapPolicy {
                        sparse: Some(SparsePolicy::default()),
                        max_ranges: Some(50),
                        ..GapPolicy::default()
                    },
                ),
                (
                    "books",
                    GapPolicy {
                        below: Some(1000),
                        start: GapStart::FromOneWithin(5),
                        ..GapPolicy::default()
                    },
                ),
                (
                    "comics",
                    GapPolicy {
                        min_entries: 3,
                        below: Some(1000),
                        fractional: Fractional::Truncate,
                        start: GapStart::FromOneWithin(5),
                        ..GapPolicy::default()
                    },
                ),
            ])
        }

        const STRING_CASES: &[&[&str]] = &[
            &[],
            &["AU"],
            &["1", "3", "3.5", "006", "AU"],
            &["1", "700"],
            &["1", "15"],
            &["1", "2", "4", "-1"],
            &["1", "150"],
            &["3.0", "1e2", "5"],
            &["2", "5", "6", "9"],
            &["1", "1.5", "2", "2.5", "4"],
            &["0.5", "1.5"],
            &["1-3", "4", "6", "7–9"],
            &["1", "998-1001"],
            &["1", "3-9223372036854775807"],
            &["6", "8"],
            &["5", "6"],
            &["0", "3"],
            &["1", "3", "1000"],
            &["1", "999"],
            &["1", "2019"],
            &["9007199254740993", "9007199254740995"],
        ];

        #[test]
        fn issues_policy_matches_mylar() {
            let policy = &policies()["issues"];
            let mut cases: Vec<Vec<String>> = STRING_CASES
                .iter()
                .map(|c| c.iter().map(|s| s.to_string()).collect())
                .collect();
            let mut dense: Vec<String> = (1..=20)
                .filter(|n| *n != 7)
                .map(|n| n.to_string())
                .collect();
            dense.push("2019".into());
            cases.push(dense.clone());
            dense.extend(["2020", "2021", "2022"].map(String::from));
            cases.push(dense);
            cases.push((0..=55).map(|i| (i * 2).to_string()).collect());
            for case in &cases {
                for total in [None, Some(2), Some(20), Some(150)] {
                    let rows: Vec<mylar::IssueState> = case
                        .iter()
                        .map(|s| mylar::IssueState {
                            number: Some(s.clone()),
                        })
                        .collect();
                    let want = mylar::gaps(&rows, total);
                    let ns: Vec<SeqNumber> = case
                        .iter()
                        .filter_map(|s| s.trim().parse::<i64>().ok())
                        .map(SeqNumber::whole)
                        .collect();
                    let size = SeriesSize {
                        entries: rows.len(),
                        expected: total,
                    };
                    let got = sequence_gaps(&ns, size, policy);
                    let conv = |r: mylar::NumberRange| NumberRange {
                        from: r.from,
                        to: r.to,
                    };
                    assert_eq!(
                        (
                            got.missing,
                            got.missing_count,
                            got.truncated,
                            got.renumbered_or_sparse,
                            got.outliers
                        ),
                        (
                            want.ranges.into_iter().map(conv).collect::<Vec<_>>(),
                            want.missing,
                            want.truncated,
                            want.renumbered_or_sparse.map(conv),
                            want.outliers
                        ),
                        "{case:?} {total:?}"
                    );
                }
            }
        }

        #[test]
        fn books_policy_matches_audiobookshelf() {
            let policy = &policies()["books"];
            for case in STRING_CASES {
                let ns = abs_input(case);
                let size = SeriesSize {
                    entries: case.len(),
                    expected: None,
                };
                assert_eq!(
                    sequence_gaps(&ns, size, policy).missing,
                    abs_gaps(case),
                    "{case:?}"
                );
            }
        }

        #[test]
        fn comics_policy_matches_komga() {
            let policy = &policies()["comics"];
            let cases: &[&[f64]] = &[
                &[],
                &[1.0, 4.0],
                &[3.0, 4.0, 7.0, 7.0],
                &[1.0, 2.5, 4.0],
                &[f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5, 1.0, 3.0],
                &[1.0, 2.0, 997.0, 999.9],
                &[1.0, 2.0, 3.0, 1000.0],
                &[0.0, 0.5, 3.0],
                &[6.0, 8.0, 8.5],
                &[5.0, 6.0, 6.0],
                &[1.0, 2019.0, 3.0],
            ];
            for case in cases {
                let ns: Vec<SeqNumber> = case.iter().map(|v| SeqNumber::Fraction(*v)).collect();
                let size = SeriesSize {
                    entries: case.len(),
                    expected: None,
                };
                assert_eq!(
                    sequence_gaps(&ns, size, policy).missing,
                    komga_gaps(case),
                    "{case:?}"
                );
            }
        }
    }
}
