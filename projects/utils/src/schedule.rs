//! Cron scheduling — the one place in the workspace that knows how orca parses
//! a cron expression and computes firing times. **Every callsite that used to
//! name `cron::Schedule` should call through here.** The backing library (the
//! `cron` crate, itself generic over chrono) is hidden: callers work entirely in
//! [`Timestamp`], never in chrono. This is an abstraction, not a re-export —
//! it's what lets `system` schedule jobs without depending on chrono at all.
//!
//! Gated by the `schedule` feature so a consumer that never schedules links
//! neither the cron parser nor its transitive datetime stack.

use crate::time::Timestamp;
use anyhow::{Context, Result};
use std::str::FromStr;

/// A parsed cron schedule. Accepts both 5-field Unix cron (`"0 3 * * *"`) and
/// the 6-field seconds form (`"0 0 3 * * *"`); a 5-field expression is treated
/// as having a `0` seconds column.
pub struct Schedule(cron::Schedule);

impl Schedule {
    /// Parse a cron expression. Errors carry the offending expression's parse
    /// failure.
    pub fn parse(expr: &str) -> Result<Self> {
        cron::Schedule::from_str(&normalize(expr))
            .map(Schedule)
            .with_context(|| format!("invalid cron expression '{expr}'"))
    }

    /// The next firing strictly after `after`, or `None` if the schedule has no
    /// further occurrences.
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        self.0
            .after(&after.inner())
            .next()
            .map(Timestamp::from_inner)
    }

    /// The next firing strictly after the current instant.
    pub fn next_from_now(&self) -> Option<Timestamp> {
        self.next_after(Timestamp::now())
    }

    /// Should this schedule have fired at least once in the window
    /// `(baseline, now]`? True when the next firing after `baseline` is at or
    /// before `now`. This is the reusable "is it due?" decision — the caller
    /// supplies the baseline (typically the later of a job's last run and some
    /// cutoff) and the current instant; it owns no I/O.
    pub fn is_due(&self, baseline: Timestamp, now: Timestamp) -> bool {
        self.next_after(baseline).is_some_and(|next| next <= now)
    }
}

/// Accept 5-field Unix cron by prepending a `0` seconds column so it parses as
/// the `cron` crate's native 6-field form.
fn normalize(expr: &str) -> String {
    if expr.split_whitespace().count() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_five_field_unix_cron() {
        let s = Schedule::parse("0 * * * *").expect("5-field cron");
        assert!(s.next_from_now().is_some());
    }

    #[test]
    fn parses_six_field_seconds_cron() {
        let s = Schedule::parse("0 0 * * * *").expect("6-field cron");
        assert!(s.next_from_now().is_some());
    }

    #[test]
    fn next_after_is_strictly_in_the_future() {
        // @hourly fires at the top of each hour; the next firing after `now`
        // is strictly after `now`.
        let s = Schedule::parse("@hourly").unwrap();
        let now = Timestamp::now();
        let next = s.next_after(now).unwrap();
        assert!(next > now);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Schedule::parse("not a cron").is_err());
    }

    #[test]
    fn is_due_true_when_firing_falls_in_window() {
        // Fires every minute. A baseline two minutes before `now` means at
        // least one firing lies in (baseline, now].
        let s = Schedule::parse("* * * * *").unwrap();
        let now = Timestamp::now();
        let baseline = now.minus(std::time::Duration::from_secs(120));
        assert!(s.is_due(baseline, now));
    }

    #[test]
    fn is_due_false_when_next_firing_is_future() {
        // @hourly with baseline = now: the next firing is the top of the next
        // hour, strictly after `now`, so it is not yet due.
        let s = Schedule::parse("@hourly").unwrap();
        let now = Timestamp::now();
        assert!(!s.is_due(now, now));
    }
}
