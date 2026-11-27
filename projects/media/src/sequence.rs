use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumberRange {
    pub from: i64,
    pub to: i64,
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
    OneIfLowestAtMost(i64),
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

pub fn sequence_gaps(
    numbers: &[f64],
    rows: usize,
    expected_total: Option<u32>,
    policy: &GapPolicy,
) -> SequenceGaps {
    let mut whole: BTreeSet<i64> = numbers
        .iter()
        .copied()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .filter(|n| policy.below.is_none_or(|b| *n < b as f64))
        .filter_map(|n| match policy.fractional {
            Fractional::Ignore => (n.fract() == 0.0).then_some(n as i64),
            Fractional::Truncate => Some(n as i64),
        })
        .collect();
    let (Some(&lo), Some(&hi)) = (whole.first(), whole.last()) else {
        return SequenceGaps::default();
    };
    let mut out = SequenceGaps::default();
    if let Some(sparse) = &policy.sparse {
        let count = expected_total.map_or(rows, |t| t as usize).max(whole.len()) as i64;
        let limit = count.saturating_mul(2).saturating_add(sparse.slack);
        if hi.saturating_sub(lo).saturating_add(1) > limit {
            let small = expected_total.is_none()
                && whole.len() < sparse.small_series
                && hi.saturating_sub(lo) < sparse.small_series_span;
            let past: Vec<i64> = match expected_total {
                Some(_) => whole.range(lo.saturating_add(limit)..).copied().collect(),
                None => Vec::new(),
            };
            if !past.is_empty() && past.len() <= sparse.max_outliers {
                whole.retain(|n| *n < lo.saturating_add(limit));
                out.outliers = past;
            } else if !small {
                out.renumbered_or_sparse = Some(NumberRange { from: lo, to: hi });
                return out;
            }
        }
    }
    let start = match policy.start {
        GapStart::OneIfLowestAtMost(at) if lo <= at => 1.min(lo),
        _ => lo,
    };
    let edges = std::iter::once(start - 1).chain(whole.iter().copied());
    for (a, b) in edges.clone().zip(edges.skip(1)) {
        let gap = b.saturating_sub(a).saturating_sub(1);
        if gap < 1 {
            continue;
        }
        out.missing_count = out.missing_count.saturating_add(gap as usize);
        if policy.max_ranges == Some(out.missing.len()) {
            out.truncated = true;
        } else {
            out.missing.push(NumberRange {
                from: a + 1,
                to: b - 1,
            });
        }
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
            start: GapStart::OneIfLowestAtMost(5),
            ..GapPolicy::default()
        }
    }

    fn comics() -> GapPolicy {
        GapPolicy {
            fractional: Fractional::Truncate,
            ..books()
        }
    }

    fn nums(ns: impl IntoIterator<Item = i64>) -> Vec<f64> {
        ns.into_iter().map(|n| n as f64).collect()
    }

    fn issue_gaps(ns: &[f64], total: Option<u32>) -> SequenceGaps {
        sequence_gaps(ns, ns.len(), total, &issues())
    }

    fn book_gaps(ns: &[f64]) -> Vec<NumberRange> {
        sequence_gaps(ns, ns.len(), None, &books()).missing
    }

    #[test]
    fn issues_gaps_are_missing_whole_numbers_between_the_ends() {
        let g = sequence_gaps(&[1.0, 3.0, 3.5, 6.0], 5, None, &issues());
        assert_eq!(g.missing, vec![r(2, 2), r(4, 5)]);
        assert_eq!(
            (g.missing_count, g.truncated, g.renumbered_or_sparse),
            (3, false, None)
        );
        assert_eq!(
            sequence_gaps(&[], 1, None, &issues()),
            SequenceGaps::default()
        );
    }

    #[test]
    fn issues_legacy_renumbering_is_sparse_not_hundreds_of_gaps() {
        let g = issue_gaps(&nums([1, 700]), None);
        assert!(g.missing.is_empty());
        assert_eq!(g.missing_count, 0);
        assert_eq!(g.renumbered_or_sparse, Some(r(1, 700)));
    }

    #[test]
    fn issues_outlier_is_split_out_and_dense_gaps_still_listed() {
        let mut ns = nums((1..=20).filter(|n| *n != 7));
        ns.push(2019.0);
        let g = issue_gaps(&ns, Some(20));
        assert_eq!(g.renumbered_or_sparse, None);
        assert_eq!(g.missing, vec![r(7, 7)]);
        assert_eq!((g.missing_count, g.outliers), (1, vec![2019]));
        ns.push(1_000_000.0);
        assert!(issue_gaps(&ns, None).renumbered_or_sparse.is_some());
    }

    #[test]
    fn issues_small_new_series_lists_gaps() {
        let g = issue_gaps(&nums([1, 15]), None);
        assert_eq!(g.renumbered_or_sparse, None);
        assert_eq!(g.missing, vec![r(2, 14)]);
        assert_eq!(g.missing_count, 13);
    }

    #[test]
    fn issues_negative_numbers_are_ignored() {
        let g = issue_gaps(&[1.0, 2.0, 4.0, -1.0], None);
        assert_eq!(g.missing, vec![r(3, 3)]);
        assert_eq!(g.missing_count, 1);
    }

    #[test]
    fn issues_expected_total_widens_the_sparse_threshold() {
        let ns = nums([1, 150]);
        assert!(issue_gaps(&ns, None).renumbered_or_sparse.is_some());
        let g = issue_gaps(&ns, Some(150));
        assert_eq!(g.renumbered_or_sparse, None);
        assert_eq!(g.missing, vec![r(2, 149)]);
        assert_eq!(g.missing_count, 148);
    }

    #[test]
    fn issues_ranges_are_capped_and_flag_truncation() {
        let n = 55;
        let g = issue_gaps(&nums((0..=n).map(|i| i * 2)), None);
        assert_eq!(g.missing.len(), 50);
        assert!(g.truncated);
        assert_eq!(g.missing_count, n as usize);
        assert_eq!(g.missing[0], r(1, 1));
    }

    #[test]
    fn books_report_missing_runs_from_one() {
        assert_eq!(
            book_gaps(&[2.0, 5.0, 6.0, 9.0]),
            vec![r(1, 1), r(3, 4), r(7, 8)]
        );
        assert!(book_gaps(&[3.0, 1.0, 2.0]).is_empty());
        assert!(book_gaps(&[1.0, 1.0, 2.0, 2.0, 2.0]).is_empty());
    }

    #[test]
    fn books_decimals_are_novellas_not_gaps() {
        assert_eq!(book_gaps(&[1.0, 1.5, 2.0, 2.5, 4.0]), vec![r(3, 3)]);
        assert!(book_gaps(&[0.5, 1.5]).is_empty());
    }

    #[test]
    fn books_omnibus_spans_cover_their_numbers() {
        assert_eq!(book_gaps(&nums([1, 2, 3, 4, 6, 7, 8, 9])), vec![r(5, 5)]);
    }

    #[test]
    fn books_gaps_start_at_lowest_when_above_five() {
        assert_eq!(book_gaps(&[6.0, 8.0]), vec![r(7, 7)]);
        assert_eq!(book_gaps(&[5.0, 6.0]), vec![r(1, 4)]);
        assert!(book_gaps(&[0.0, 1.0]).is_empty());
    }

    #[test]
    fn books_numbers_from_one_thousand_are_ignored() {
        assert_eq!(book_gaps(&[1.0, 3.0, 1000.0]), vec![r(2, 2)]);
        assert_eq!(book_gaps(&[1.0, 999.0]), vec![r(2, 998)]);
        assert!(book_gaps(&[1.0, 2019.0]).is_empty());
    }

    #[test]
    fn books_missing_count_sums_ranges() {
        let g = sequence_gaps(&[1.0, 4.0], 2, None, &books());
        assert_eq!(g.missing_count, 2);
    }

    #[test]
    fn huge_numbers_saturate_instead_of_overflowing() {
        let open = GapPolicy::default();
        let g = sequence_gaps(&[0.0, 1e300], 2, None, &open);
        assert_eq!(g.missing, vec![r(1, i64::MAX - 1)]);
        let g = sequence_gaps(&[0.0, 1e300], 2, Some(u32::MAX), &issues());
        assert_eq!(g.outliers, vec![i64::MAX]);
    }

    #[test]
    fn comics_truncate_fractions_and_fill_from_one() {
        let g = sequence_gaps(&[3.0, 4.0, 7.0, 7.0], 4, None, &comics());
        assert_eq!(g.missing, vec![r(1, 2), r(5, 6)]);
        assert_eq!(g.missing_count, 4);
        assert_eq!(
            sequence_gaps(&[1.0, 2.5, 4.0], 3, None, &comics()).missing,
            vec![r(3, 3)]
        );
    }
}
