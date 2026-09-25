//! Projects when a filling resource runs out, from a series of usage samples.
//!
//! A current-usage threshold ("warn at 85%") is the check everyone writes, and
//! it is the wrong one twice over: it screams about a filesystem that has sat at
//! 86% for two years, and it stays silent on one that went 20% → 60% this week
//! and has four days left. What an operator needs is the *derivative* — is this
//! filling, and when does it run out.
//!
//! This is the analysis half only. It takes samples and returns a verdict, with
//! no knowledge of filesystems, guests, or thin pools, so the same function
//! serves a host filesystem, an LXC rootfs, an LVM-thin pool, or a log
//! directory. Callers supply samples from wherever they have them.
//!
//! Method is ordinary least squares on (time, used) — not last-minus-first.
//! A single deep dip (a log rotation, an fstrim) swings a two-point slope wildly
//! and would fire a bogus "full in 3 hours"; a fitted line over the whole window
//! is dominated by the trend rather than by either endpoint.

/// One usage observation. `used` and `total` share whatever unit the caller
/// prefers (bytes, MiB, percent-of-pool) — only their ratio and slope are used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Unix seconds.
    pub ts: i64,
    pub used: f64,
    pub total: f64,
}

/// Which way a resource is moving, and how urgently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    /// Not enough signal to judge — too few samples, no elapsed time, or a
    /// degenerate `total`. Deliberately distinct from `Flat`: "we don't know"
    /// must never render as "it's fine".
    Unknown,
    /// Usage is falling or flat within noise.
    Stable,
    /// Filling, but projected to stay within bounds past the caller's horizon.
    Filling,
    /// Filling and projected to reach capacity inside the horizon.
    FillingFast,
}

/// Outcome of a trend fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    pub trend: Trend,
    /// Fraction of capacity used at the newest sample, `0.0..=1.0` (can exceed
    /// 1.0 for an over-committed thin pool, which is real and worth surfacing).
    pub used_fraction: f64,
    /// Fitted growth in `used` units per day. Negative = draining.
    pub per_day: f64,
    /// Seconds until `used` reaches `total` at the fitted rate. `None` when not
    /// filling, or when the fit is not usable.
    pub seconds_to_full: Option<f64>,
}

impl Projection {
    /// Days until full, for display. `None` mirrors [`Self::seconds_to_full`].
    pub fn days_to_full(&self) -> Option<f64> {
        self.seconds_to_full.map(|s| s / 86_400.0)
    }
}

/// Growth below this fraction of capacity per day counts as noise rather than a
/// trend. 0.1% of capacity/day is ~2.7 years to fill — no operator wants to be
/// paged for that, and sampling jitter alone clears a tighter bound.
const NOISE_FLOOR_FRACTION_PER_DAY: f64 = 0.001;

/// Fit a trend and project time-to-full.
///
/// `horizon_days` is what separates [`Trend::Filling`] from
/// [`Trend::FillingFast`] — the caller's "soon enough that I must act". Samples
/// may arrive in any order; they are sorted internally, since the history ring
/// hands back newest-first while a fit wants oldest-first.
pub fn project(samples: &[Sample], horizon_days: f64) -> Projection {
    let unknown = |used_fraction: f64| Projection {
        trend: Trend::Unknown,
        used_fraction,
        per_day: 0.0,
        seconds_to_full: None,
    };

    // Drop degenerate rows rather than letting a zero `total` produce inf/NaN
    // that silently poisons the fit.
    let mut pts: Vec<Sample> = samples
        .iter()
        .copied()
        .filter(|s| s.total > 0.0 && s.used.is_finite() && s.total.is_finite())
        .collect();
    if pts.is_empty() {
        return unknown(0.0);
    }
    pts.sort_by_key(|s| s.ts);

    let last = *pts.last().expect("non-empty");
    let used_fraction = last.used / last.total;

    // One sample is a level, not a trend. Saying `Unknown` here is the whole
    // point: a brand-new host must not read as healthy-and-stable.
    if pts.len() < 2 {
        return unknown(used_fraction);
    }
    let span = (last.ts - pts[0].ts) as f64;
    if span <= 0.0 {
        // Every sample at one instant — a duplicated tick, not a time series.
        return unknown(used_fraction);
    }

    // OLS slope of `used` against seconds, with time re-based to the first
    // sample so the sums stay small (unix seconds squared overflows f64
    // precision long before it overflows range).
    let n = pts.len() as f64;
    let t0 = pts[0].ts;
    let xs: Vec<f64> = pts.iter().map(|s| (s.ts - t0) as f64).collect();
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = pts.iter().map(|s| s.used).sum::<f64>() / n;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for (x, s) in xs.iter().zip(pts.iter()) {
        sxy += (x - mean_x) * (s.used - mean_y);
        sxx += (x - mean_x) * (x - mean_x);
    }
    if sxx <= 0.0 {
        return unknown(used_fraction);
    }
    let per_second = sxy / sxx;
    let per_day = per_second * 86_400.0;

    // Compare growth against capacity, not against absolute units: 1 GiB/day is
    // trivial on a 20 TiB array and terminal on a 4 GiB rootfs.
    if per_day <= last.total * NOISE_FLOOR_FRACTION_PER_DAY {
        return Projection {
            trend: Trend::Stable,
            used_fraction,
            per_day,
            seconds_to_full: None,
        };
    }

    let remaining = (last.total - last.used).max(0.0);
    let seconds_to_full = remaining / per_second;
    let trend = if seconds_to_full <= horizon_days * 86_400.0 {
        Trend::FillingFast
    } else {
        Trend::Filling
    };
    Projection {
        trend,
        used_fraction,
        per_day,
        seconds_to_full: Some(seconds_to_full),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    /// `total` in GiB, one sample per day, `used` walking the given values.
    fn series(total: f64, used: &[f64]) -> Vec<Sample> {
        used.iter()
            .enumerate()
            .map(|(i, &u)| Sample {
                ts: i as i64 * DAY,
                used: u,
                total,
            })
            .collect()
    }

    #[test]
    fn steady_growth_projects_a_correct_time_to_full() {
        // 100 GiB total, +5 GiB/day, at 80 used => 20 remaining => 4 days.
        let s = series(100.0, &[60.0, 65.0, 70.0, 75.0, 80.0]);
        let p = project(&s, 7.0);
        assert_eq!(p.trend, Trend::FillingFast);
        assert!((p.per_day - 5.0).abs() < 1e-9, "per_day was {}", p.per_day);
        let days = p.days_to_full().expect("filling => projection");
        assert!((days - 4.0).abs() < 1e-9, "days was {days}");
    }

    /// The case a static threshold gets backwards: parked high, going nowhere.
    #[test]
    fn a_high_but_flat_filesystem_is_stable_not_an_alert() {
        let s = series(100.0, &[86.0, 86.1, 86.0, 85.9, 86.0]);
        let p = project(&s, 7.0);
        assert_eq!(p.trend, Trend::Stable);
        assert!(p.used_fraction > 0.85, "still reports the high level");
        assert_eq!(p.seconds_to_full, None, "not filling => no projection");
    }

    /// The other half: low now, but will hit the wall inside the horizon.
    #[test]
    fn a_low_but_fast_filling_filesystem_is_caught() {
        let s = series(100.0, &[20.0, 30.0, 40.0, 50.0, 60.0]);
        let p = project(&s, 7.0);
        assert_eq!(p.trend, Trend::FillingFast, "40 left at 10/day = 4 days");
        assert!(
            p.used_fraction < 0.65,
            "a level check would have missed this"
        );
    }

    #[test]
    fn slow_growth_beyond_the_horizon_is_filling_not_fast() {
        // +1 GiB/day with 60 remaining = 60 days, well past a 7-day horizon.
        let s = series(100.0, &[36.0, 37.0, 38.0, 39.0, 40.0]);
        let p = project(&s, 7.0);
        assert_eq!(p.trend, Trend::Filling);
        assert!(p.days_to_full().expect("projection") > 55.0);
    }

    #[test]
    fn draining_is_stable_and_never_projects_a_full_date() {
        let s = series(100.0, &[90.0, 80.0, 70.0, 60.0]);
        let p = project(&s, 7.0);
        assert_eq!(p.trend, Trend::Stable);
        assert!(p.per_day < 0.0, "slope must show the drain");
        assert_eq!(p.seconds_to_full, None);
    }

    /// A reclaim dip mid-window must not dominate the verdict.
    #[test]
    fn a_single_reclaim_dip_does_not_flip_a_real_upward_trend() {
        let s = series(100.0, &[50.0, 55.0, 60.0, 20.0, 70.0, 75.0, 80.0]);
        let p = project(&s, 30.0);
        assert!(
            matches!(p.trend, Trend::Filling | Trend::FillingFast),
            "trend was {:?}; the dip won",
            p.trend
        );
        assert!(p.per_day > 0.0);
    }

    /// Pins the FIT ITSELF, not just its sign: a reclaim landing on the newest
    /// sample is the endpoint with the most leverage, and `last - first` is
    /// nothing but endpoints — it reads this steadily-filling volume as
    /// *draining* and stays silent. Least squares keeps 19 real samples against
    /// one outlier and still reports the growth.
    ///
    /// This is the test that makes the OLS choice load-bearing; a short series
    /// cannot distinguish the two methods, because with few points one dip
    /// dominates either fit.
    #[test]
    fn a_reclaim_on_the_newest_sample_does_not_erase_the_trend() {
        // +1 GiB/day for 19 days, then an fstrim drops 69 -> 30.
        let mut used: Vec<f64> = (0..19).map(|i| 50.0 + i as f64).collect();
        used.push(30.0);
        let s = series(100.0, &used);

        // What `last - first` would conclude: (30 - 50) / 19 days < 0.
        let naive_per_day = (used[19] - used[0]) / 19.0;
        assert!(naive_per_day < 0.0, "premise: the naive slope is negative");

        let p = project(&s, 7.0);
        assert!(
            p.per_day > 0.0,
            "OLS must still see the climb; got {:+.3}/day",
            p.per_day
        );
        assert_eq!(p.trend, Trend::Filling, "must not report Stable");
        assert!(p.days_to_full().is_some(), "a projection must survive");
    }

    #[test]
    fn noise_below_the_floor_is_stable() {
        // ~0.02% of capacity per day — decades to fill. Not an alert.
        let s = series(100.0, &[50.0, 50.01, 50.02, 50.03, 50.04]);
        assert_eq!(project(&s, 7.0).trend, Trend::Stable);
    }

    /// "Don't know" must be distinguishable from "fine" — this is what keeps a
    /// fresh host from reporting healthy before it has any history.
    #[test]
    fn too_little_signal_is_unknown_not_stable() {
        assert_eq!(project(&[], 7.0).trend, Trend::Unknown);
        assert_eq!(project(&series(100.0, &[50.0]), 7.0).trend, Trend::Unknown);
        // All samples at the same instant: a repeated tick, not a series.
        let dup = vec![
            Sample {
                ts: 5,
                used: 10.0,
                total: 100.0,
            },
            Sample {
                ts: 5,
                used: 90.0,
                total: 100.0,
            },
        ];
        assert_eq!(project(&dup, 7.0).trend, Trend::Unknown);
    }

    #[test]
    fn a_single_sample_still_reports_its_level() {
        let p = project(&series(200.0, &[150.0]), 7.0);
        assert_eq!(p.trend, Trend::Unknown);
        assert!(
            (p.used_fraction - 0.75).abs() < 1e-9,
            "level is still useful"
        );
    }

    #[test]
    fn zero_and_nonfinite_totals_are_dropped_not_propagated_as_nan() {
        let s = vec![
            Sample {
                ts: 0,
                used: 10.0,
                total: 0.0,
            },
            Sample {
                ts: DAY,
                used: 20.0,
                total: f64::NAN,
            },
            Sample {
                ts: 2 * DAY,
                used: 30.0,
                total: 100.0,
            },
            Sample {
                ts: 3 * DAY,
                used: 40.0,
                total: 100.0,
            },
        ];
        let p = project(&s, 7.0);
        assert!(p.used_fraction.is_finite(), "NaN leaked into the verdict");
        assert!(p.per_day.is_finite());
        assert_eq!(
            p.trend,
            Trend::FillingFast,
            "the two valid points still fit"
        );
    }

    #[test]
    fn samples_may_arrive_newest_first() {
        // The history ring returns newest-first; the fit must not care.
        let mut s = series(100.0, &[60.0, 65.0, 70.0, 75.0, 80.0]);
        s.reverse();
        let p = project(&s, 7.0);
        assert!(
            (p.per_day - 5.0).abs() < 1e-9,
            "sign flipped on reversed input"
        );
        assert!((p.days_to_full().expect("filling") - 4.0).abs() < 1e-9);
    }

    /// An over-committed thin pool genuinely reports >100%; clamping would hide
    /// the worst case there is.
    #[test]
    fn over_full_is_reported_rather_than_clamped() {
        let s = series(100.0, &[95.0, 100.0, 105.0]);
        let p = project(&s, 7.0);
        assert!(p.used_fraction > 1.0, "got {}", p.used_fraction);
        assert_eq!(
            p.seconds_to_full,
            Some(0.0),
            "already past capacity = no time left"
        );
        assert_eq!(p.trend, Trend::FillingFast);
    }
}
