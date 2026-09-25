//! Core diagnostics provider: filesystems trending full.
//!
//! The check everyone writes is "warn above 85%", and it is wrong in both
//! directions. It pages about a filesystem parked at 86% for two years, and it
//! says nothing about one that went 20% → 60% this week with four days left.
//! This provider judges the *derivative* instead, over the `system` history ring
//! — see [`utils::capacity_trend`] for the fit and why it is least-squares
//! rather than last-minus-first (an fstrim or log rotation mid-window would
//! otherwise read as draining).
//!
//! ## Scope
//!
//! Covers the filesystem hosting `~/.orca` on whichever host this runs on. It is
//! peer-reachable the same way every diagnostics verb is, so a controller gets
//! each host's verdict by fanning `diagnostics.diagnose` over the mesh; there is
//! no attempt to reach *inside* guests from here. Guest and thin-pool series
//! belong to the proxmox/docker plugins, which is why the projection lives in
//! `utils` as a pure function they can call with their own samples.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use contract::BoxFuture;
use contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome, Severity,
};
use utils::capacity_trend::{self, Projection, Sample, Trend};

/// Registry name. Stable — operators type it as `--provider`.
pub const PROVIDER_NAME: &str = "capacity";

/// Samples to fit over. The ring is written once per refresher tick, so this is
/// a window of recent behaviour rather than all history — a filesystem that
/// filled last month and has been flat since should read Stable today.
const WINDOW: usize = 500;

/// "Soon enough that I must act." A week is the useful line for a homelab: long
/// enough to order a disk or prune, short enough that the warning still matters.
const HORIZON_DAYS: f64 = 7.0;

/// Level at which a *non-growing* filesystem is still worth mentioning. Being
/// nearly full is a real constraint even with a flat trend — it just is not the
/// same urgency as filling.
const HIGH_WATER: f64 = 0.90;

/// Build a finding from a projection, or `None` when there is nothing to say.
///
/// Split out from the provider so the decision table is unit-testable without a
/// database — the history read is the untestable part, the judgement is not.
fn finding_for(p: &Projection) -> Option<Finding> {
    let pct = p.used_fraction * 100.0;
    match p.trend {
        // No series yet (fresh install, retention 0). Silence is right: an
        // Info finding on every new host trains operators to ignore the report.
        Trend::Unknown => None,
        Trend::Stable if p.used_fraction >= HIGH_WATER => Some(Finding {
            id: "fs-high-water".to_string(),
            provider: PROVIDER_NAME.to_string(),
            severity: Severity::Warn,
            title: format!("Filesystem {pct:.0}% full (not growing)"),
            detail: format!(
                "The filesystem hosting ~/.orca is {pct:.1}% used but is not trending \
                 upward ({:+.2} GiB/day over the last {} samples), so there is no \
                 projected exhaustion date. Headroom is thin: a large restore or \
                 image pull could still fill it.",
                p.per_day, WINDOW
            ),
            repair: None,
        }),
        Trend::Stable => None,
        Trend::Filling => {
            // Growing but beyond the horizon. Worth recording, not worth waking
            // anyone — Info is the honest severity.
            let days = p.days_to_full()?;
            Some(Finding {
                id: "fs-filling".to_string(),
                provider: PROVIDER_NAME.to_string(),
                severity: Severity::Info,
                title: format!("Filesystem filling: ~{days:.0} days of headroom"),
                detail: format!(
                    "{pct:.1}% used, growing {:+.2} GiB/day. Projected full in \
                     {days:.0} days — outside the {HORIZON_DAYS:.0}-day action \
                     horizon, so this is a note, not an alert.",
                    p.per_day
                ),
                repair: None,
            })
        }
        Trend::FillingFast => {
            let days = p.days_to_full()?;
            Some(Finding {
                id: "fs-filling-fast".to_string(),
                provider: PROVIDER_NAME.to_string(),
                severity: Severity::Crit,
                title: format!("Filesystem full in ~{days:.1} days"),
                detail: format!(
                    "{pct:.1}% used and growing {:+.2} GiB/day — projected to reach \
                     capacity in {days:.1} days, inside the {HORIZON_DAYS:.0}-day \
                     horizon. A full filesystem takes the daemon and every service \
                     writing through it down at once.",
                    p.per_day
                ),
                // No repair: reclaiming space means deciding what to delete, and
                // that is never orca's call to make unattended.
                repair: None,
            })
        }
    }
}

/// Pull `(ts, used, total)` triples out of history points that carry disk data.
///
/// Points predating the disk fields simply lack them, so an upgraded host fits
/// over whatever it has and reports `Unknown` until the ring refills — which is
/// correct, and the reason `Unknown` is distinct from `Stable`.
fn samples_from(points: &[crate::system_info_types::SystemHistoryPoint]) -> Vec<Sample> {
    points
        .iter()
        .filter_map(|p| {
            Some(Sample {
                ts: p.ts,
                used: p.fs_used_gb? as f64,
                total: p.fs_total_gb? as f64,
            })
        })
        .collect()
}

/// Reports filesystems trending toward full. See the module docs for scope.
pub struct CapacityDiagnostics;

impl DiagnosticsProvider for CapacityDiagnostics {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn diagnose(&self, _args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        Box::pin(async move {
            let points = crate::system_info::history::read_tail(WINDOW);
            let samples = samples_from(&points);
            let p = capacity_trend::project(&samples, HORIZON_DAYS);
            Ok(finding_for(&p).into_iter().collect())
        })
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        Box::pin(async move {
            Err(anyhow!(
                "{PROVIDER_NAME} has no automatic repair for {:?}: reclaiming space \
                 means choosing what to delete, which orca must not do unattended",
                args.repair_id
            ))
        })
    }
}

/// Register this provider. Idempotent — `register_provider` replaces by name.
pub fn register() {
    contract::diagnostics::register_provider(Arc::new(CapacityDiagnostics));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_info_types::SystemHistoryPoint;

    const DAY: i64 = 86_400;

    fn proj(total: f64, used: &[f64]) -> Projection {
        let s: Vec<Sample> = used
            .iter()
            .enumerate()
            .map(|(i, &u)| Sample {
                ts: i as i64 * DAY,
                used: u,
                total,
            })
            .collect();
        capacity_trend::project(&s, HORIZON_DAYS)
    }

    #[test]
    fn filling_inside_the_horizon_is_crit() {
        // 100 GiB, +10/day, at 60 => 4 days left.
        let f = finding_for(&proj(100.0, &[20.0, 30.0, 40.0, 50.0, 60.0])).expect("must report");
        assert_eq!(f.severity, Severity::Crit);
        assert_eq!(f.id, "fs-filling-fast");
        assert!(f.title.contains("4.0 days"), "title was {:?}", f.title);
        assert!(f.repair.is_none(), "orca must not choose what to delete");
    }

    /// The case a level threshold inverts: low usage, terminal trajectory. A
    /// "warn above 85%" check reports nothing here.
    #[test]
    fn a_low_but_fast_filling_filesystem_is_still_crit() {
        let p = proj(100.0, &[20.0, 30.0, 40.0, 50.0, 60.0]);
        assert!(p.used_fraction < 0.65, "only 60% used");
        assert_eq!(
            finding_for(&p).expect("must report").severity,
            Severity::Crit
        );
    }

    #[test]
    fn filling_beyond_the_horizon_is_info_not_crit() {
        // +1/day with 60 remaining = 60 days.
        let f = finding_for(&proj(100.0, &[36.0, 37.0, 38.0, 39.0, 40.0])).expect("must report");
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.id, "fs-filling");
    }

    /// The other inversion: parked high for years is not an emergency, but the
    /// thin headroom is still worth saying once.
    #[test]
    fn high_but_flat_is_a_warn_about_headroom_not_a_projection() {
        let f = finding_for(&proj(100.0, &[95.0, 95.1, 95.0, 94.9, 95.0])).expect("must report");
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.id, "fs-high-water");
        assert!(f.detail.contains("not trending upward"));
    }

    #[test]
    fn comfortable_and_flat_reports_nothing() {
        assert!(
            finding_for(&proj(100.0, &[40.0, 40.1, 40.0, 39.9, 40.0])).is_none(),
            "a healthy filesystem must not add noise to the report"
        );
    }

    #[test]
    fn draining_reports_nothing() {
        assert!(finding_for(&proj(100.0, &[95.0, 85.0, 75.0, 65.0])).is_none());
    }

    /// No history yet must be silent rather than a finding on every fresh host.
    #[test]
    fn unknown_reports_nothing() {
        assert_eq!(proj(100.0, &[50.0]).trend, Trend::Unknown);
        assert!(finding_for(&proj(100.0, &[50.0])).is_none());
    }

    #[test]
    fn points_without_disk_fields_are_skipped_not_zeroed() {
        let mk = |ts: i64, fs: Option<(u64, u64)>| SystemHistoryPoint {
            ts,
            fs_total_gb: fs.map(|(t, _)| t),
            fs_used_gb: fs.map(|(_, u)| u),
            ..Default::default()
        };
        // Pre-upgrade points carry no disk data; treating them as used=0 would
        // invent a huge fake growth spike.
        let pts = vec![
            mk(0, None),
            mk(DAY, None),
            mk(2 * DAY, Some((100, 50))),
            mk(3 * DAY, Some((100, 55))),
        ];
        let s = samples_from(&pts);
        assert_eq!(s.len(), 2, "only the points with disk data are fitted");
        assert!(capacity_trend::project(&s, HORIZON_DAYS).per_day > 0.0);
    }

    #[test]
    fn an_empty_ring_yields_unknown_and_no_finding() {
        let s = samples_from(&[]);
        let p = capacity_trend::project(&s, HORIZON_DAYS);
        assert_eq!(p.trend, Trend::Unknown);
        assert!(finding_for(&p).is_none());
    }

    #[tokio::test]
    async fn register_is_idempotent_and_the_provider_is_reachable() {
        register();
        register();
        let n = contract::diagnostics::providers()
            .iter()
            .filter(|p| p.name() == PROVIDER_NAME)
            .count();
        assert_eq!(n, 1, "re-register must replace, not duplicate");
    }

    #[tokio::test]
    async fn repair_is_an_honest_error_not_a_fake_success() {
        let r = CapacityDiagnostics
            .repair(RepairArgs {
                provider: PROVIDER_NAME.to_string(),
                repair_id: "fs-filling-fast".to_string(),
                confirm: true,
            })
            .await;
        assert!(r.is_err(), "must not claim to have reclaimed space");
    }
}
