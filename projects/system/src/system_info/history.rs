//! Per-host rolling history for system metrics — the `system` time-series.
//!
//! Backed by the generic, ENCRYPTED history subsystem (`db::metrics`, a separate
//! SQLCipher `~/.orca/metrics.db`), NOT the plaintext JSONL ring this replaced
//! and NOT `orca.db`. `append` records one sample per refresher tick; readers
//! pull a cursor-paginated window. Retention is per-series (series `system`)
//! with a system-wide default; a positive age cap prunes older samples at write
//! time, `retention = 0` keeps only the latest sample.
//!
//! The old `~/.orca/history/system.jsonl` (unencrypted) is imported once into
//! `metrics.db` and then deleted — see [`migrate_legacy_jsonl_once`].

use crate::system_info_types::{GpuPoint, SystemHistoryPoint, SystemInfoReport};
use std::path::PathBuf;
use std::sync::Once;

/// The time-series name this host records system metrics under.
pub const SERIES: &str = "system";

/// Fallback age cap (seconds) when no operator override is set AND the DB pool
/// isn't available (early startup, tests).
const FALLBACK_MAX_AGE_SECS: i64 = 24 * 60 * 60;

/// Resolve the age cap (seconds) for the `system` series: a per-series override
/// in `metrics.db` wins; otherwise the operator-set host default; otherwise
/// [`FALLBACK_MAX_AGE_SECS`].
fn current_max_age_secs() -> i64 {
    if let Ok(Some(ms)) = db::metrics::retention(SERIES) {
        return ms / 1000;
    }
    db::pool::with_pooled_or_open(|conn| Ok(db::host_status::retention_seconds(conn)))
        .ok()
        .unwrap_or(FALLBACK_MAX_AGE_SECS)
}

/// Derive a history point from a fresh snapshot. Returns `None` when the
/// snapshot lacks both CPU and memory (no signal worth persisting).
pub fn point_from(snap: &SystemInfoReport) -> Option<SystemHistoryPoint> {
    let ts = snap.snapshot_at_unix?;
    if snap.cpu_usage_percent.is_none() && snap.mem_used_mb.is_none() && snap.gpus.is_empty() {
        return None;
    }
    Some(SystemHistoryPoint {
        ts,
        cpu_percent: snap.cpu_usage_percent,
        mem_used_mb: snap.mem_used_mb,
        mem_total_mb: snap.mem_total_mb,
        process_rss_mb: snap.process_rss_mb,
        gpus: snap
            .gpus
            .iter()
            .map(|g| GpuPoint {
                name: g.name.clone(),
                utilization_percent: g.utilization_percent,
                vram_used_mb: g.vram_used_mb,
                vram_total_mb: g.vram_total_mb,
                temperature_c: g.temperature_c,
            })
            .collect(),
    })
}

/// Append one sample to the encrypted `system` series, then enforce retention:
/// a positive age cap prunes older samples; `retention = 0` keeps only the
/// latest. Best-effort — a metrics-store failure is logged, never propagated
/// (losing a datapoint must not break a refresher tick).
pub fn append(point: &SystemHistoryPoint) {
    migrate_legacy_jsonl_once();
    let ts_ms = point.ts.saturating_mul(1000);
    if let Err(e) = db::metrics::record_json(SERIES, ts_ms, point) {
        tracing::warn!(error=%e, "history record failed");
        return;
    }
    let max_age = current_max_age_secs();
    if max_age > 0 {
        let cutoff_ms = utils::time::now().unix_seconds().saturating_sub(max_age) * 1000;
        if let Err(e) = db::metrics::sweep(SERIES, cutoff_ms) {
            tracing::warn!(error=%e, "history retention sweep failed");
        }
    } else {
        // retention = 0 ⇒ "no persistent history": keep only the sample we
        // just wrote so the current datapoint survives, drop everything older.
        if let Err(e) = db::metrics::sweep(SERIES, ts_ms) {
            tracing::warn!(error=%e, "history truncate failed");
        }
    }
}

/// Read the last `n` history points, oldest-first (the order the JSONL ring
/// returned, preserved for existing callers). Retention is enforced at write
/// time, so anything still stored is fair to surface.
pub fn read_tail(n: usize) -> Vec<SystemHistoryPoint> {
    migrate_legacy_jsonl_once();
    let page = match db::metrics::query(SERIES, 0, i64::MAX, n) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error=%e, "history read failed");
            return Vec::new();
        }
    };
    // `query` returns newest-first; existing callers expect oldest-first.
    page.samples
        .into_iter()
        .rev()
        .filter_map(|s| serde_json::from_str::<SystemHistoryPoint>(&s.payload).ok())
        .collect()
}

fn legacy_jsonl_path() -> Option<PathBuf> {
    Some(
        files::ops::orca_home()?
            .join("history")
            .join("system.jsonl"),
    )
}

/// One-time migration: import the pre-metrics `system.jsonl` ring into the
/// encrypted store, then DELETE it (it was plaintext on disk). Idempotent and
/// best-effort — runs at most once per process, and only does work if the file
/// still exists.
fn migrate_legacy_jsonl_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Some(path) = legacy_jsonl_path() else {
            return;
        };
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return; // absent (already migrated) — the common case.
        };
        let mut imported = 0usize;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(p) = serde_json::from_str::<SystemHistoryPoint>(line) {
                let ts_ms = p.ts.saturating_mul(1000);
                if db::metrics::record_json(SERIES, ts_ms, &p).is_ok() {
                    imported += 1;
                }
            }
        }
        // Remove the plaintext ring regardless of import count — its data now
        // lives (encrypted) in metrics.db, and leaving it would be an
        // unencrypted copy of the same series.
        std::fs::remove_file(&path).ok();
        if imported > 0 {
            tracing::info!(imported, "migrated legacy history JSONL into metrics.db");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_info_types::SystemInfoReport;

    #[test]
    fn point_from_skips_empty_snapshot() {
        let snap = SystemInfoReport {
            snapshot_at_unix: Some(1000),
            ..Default::default()
        };
        assert!(point_from(&snap).is_none());
    }

    #[test]
    fn point_from_keeps_with_cpu() {
        let snap = SystemInfoReport {
            snapshot_at_unix: Some(1000),
            cpu_usage_percent: Some(12.5),
            ..Default::default()
        };
        let p = point_from(&snap).unwrap();
        assert_eq!(p.ts, 1000);
        assert_eq!(p.cpu_percent, Some(12.5));
    }
}
