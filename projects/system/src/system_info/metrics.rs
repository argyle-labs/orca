//! Server-side percent computations for `SystemInfoReport`.
//!
//! Ported verbatim from the (now-deleted) frontend `sysMetrics.ts` so every
//! surface — UI, CLI, REST, MCP — reads identical numbers from the same
//! field instead of re-implementing the math per client. See
//! [[feedback-thin-ui-rust-first]].
//!
//! Edge-case parity with the TS original:
//! - `mem_percent`: returns `None` when `mem_total_mb` is missing OR zero,
//!   OR when `mem_used_mb` is missing. (TS returned `0` for these; the
//!   server expresses "no signal" as `None` so the UI doesn't render a
//!   misleading 0 % bar.)
//! - `load_percent`: returns `None` when `load_avg_1` is missing OR
//!   `cpu_logical` is missing/zero. (TS already returned `null` here.)
//! - `cpu_percent`: passthrough of `cpu_usage_percent`. (TS already
//!   returned `null` when absent.)
//!
//! All three clamp to `<= 100` so a momentary overshoot (load > cores)
//! doesn't break a 0–100 chart axis.

use crate::system_info_types::SystemInfoReport;

/// Memory usage 0–100 %. See module docs for edge-case semantics.
pub fn mem_percent(sys: &SystemInfoReport) -> Option<f32> {
    let total = sys.mem_total_mb?;
    let used = sys.mem_used_mb?;
    if total == 0 {
        return None;
    }
    let pct = (used as f32 / total as f32) * 100.0;
    Some(pct.min(100.0))
}

/// 1-minute load average as a percent of logical CPU count. See module
/// docs for edge-case semantics.
pub fn load_percent(sys: &SystemInfoReport) -> Option<f32> {
    let load = sys.load_avg_1?;
    let cpus = sys.cpu_logical?;
    if cpus == 0 {
        return None;
    }
    let pct = (load as f32 / cpus as f32) * 100.0;
    Some(pct.min(100.0))
}

/// Aggregate CPU usage 0–100 % — passthrough of `cpu_usage_percent`.
pub fn cpu_percent(sys: &SystemInfoReport) -> Option<f32> {
    sys.cpu_usage_percent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SystemInfoReport {
        SystemInfoReport::default()
    }

    // ── mem_percent ──

    #[test]
    fn mem_percent_normal() {
        let mut s = base();
        s.mem_total_mb = Some(1000);
        s.mem_used_mb = Some(250);
        assert_eq!(mem_percent(&s), Some(25.0));
    }

    #[test]
    fn mem_percent_clamps_to_100() {
        let mut s = base();
        s.mem_total_mb = Some(1000);
        s.mem_used_mb = Some(2000);
        assert_eq!(mem_percent(&s), Some(100.0));
    }

    #[test]
    fn mem_percent_zero_total_is_none() {
        let mut s = base();
        s.mem_total_mb = Some(0);
        s.mem_used_mb = Some(0);
        assert_eq!(mem_percent(&s), None);
    }

    #[test]
    fn mem_percent_missing_total_is_none() {
        let mut s = base();
        s.mem_used_mb = Some(100);
        assert_eq!(mem_percent(&s), None);
    }

    #[test]
    fn mem_percent_missing_used_is_none() {
        let mut s = base();
        s.mem_total_mb = Some(1000);
        assert_eq!(mem_percent(&s), None);
    }

    // ── load_percent ──

    #[test]
    fn load_percent_normal() {
        let mut s = base();
        s.load_avg_1 = Some(2.0);
        s.cpu_logical = Some(8);
        assert_eq!(load_percent(&s), Some(25.0));
    }

    #[test]
    fn load_percent_clamps_to_100() {
        let mut s = base();
        s.load_avg_1 = Some(16.0);
        s.cpu_logical = Some(8);
        assert_eq!(load_percent(&s), Some(100.0));
    }

    #[test]
    fn load_percent_zero_cpus_is_none() {
        let mut s = base();
        s.load_avg_1 = Some(1.0);
        s.cpu_logical = Some(0);
        assert_eq!(load_percent(&s), None);
    }

    #[test]
    fn load_percent_missing_load_is_none() {
        let mut s = base();
        s.cpu_logical = Some(8);
        assert_eq!(load_percent(&s), None);
    }

    #[test]
    fn load_percent_missing_cpus_is_none() {
        let mut s = base();
        s.load_avg_1 = Some(1.0);
        assert_eq!(load_percent(&s), None);
    }

    // ── cpu_percent ──

    #[test]
    fn cpu_percent_passthrough() {
        let mut s = base();
        s.cpu_usage_percent = Some(42.5);
        assert_eq!(cpu_percent(&s), Some(42.5));
    }

    #[test]
    fn cpu_percent_missing_is_none() {
        assert_eq!(cpu_percent(&base()), None);
    }
}
