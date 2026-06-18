//! Per-host rolling history ring for system metrics.
//!
//! Lives at `~/.orca/history/system.jsonl` as append-only JSONL. The
//! background refresher writes one line per tick; readers tail the last N
//! lines and filter by age.
//!
//! Files-not-rows by design — see [[project-db-size-and-retention]]. The
//! ring is size-capped (default 5 MiB) and age-capped (default 24 h); when
//! the file exceeds either bound the oldest half is dropped via rewrite.

use crate::system_info_types::{GpuPoint, SystemHistoryPoint, SystemInfoReport};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Fallback bytes cap when no per-peer override is configured AND the DB
/// pool isn't available (early startup, tests). Operator-set caps via
/// `system.retention.set max_mb=N` take precedence at runtime.
const FALLBACK_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Fallback age cap (seconds). Same precedence rule as `FALLBACK_MAX_BYTES`.
const FALLBACK_MAX_AGE_SECS: i64 = 24 * 60 * 60;

/// Resolve the size cap for the local host's JSONL ring. Honors the
/// per-peer `max_mb` policy when set; falls back to [`FALLBACK_MAX_BYTES`]
/// when no override exists or the DB pool isn't initialized.
fn current_max_bytes() -> u64 {
    let local = crate::host_identity::machine_id_short().to_string();
    db::pool::with_pooled_or_open(|conn| Ok(db::host_status::retention_max_bytes(conn, &local)))
        .ok()
        .flatten()
        .map(|b| b as u64)
        .unwrap_or(FALLBACK_MAX_BYTES)
}

/// Resolve the age cap (seconds) for the local host's JSONL ring.
fn current_max_age_secs() -> i64 {
    let local = crate::host_identity::machine_id_short().to_string();
    db::pool::with_pooled_or_open(|conn| Ok(db::host_status::retention_seconds(conn, &local)))
        .ok()
        .unwrap_or(FALLBACK_MAX_AGE_SECS)
}

fn history_path() -> Option<PathBuf> {
    let home = files::ops::orca_home()?;
    let dir = home.join("history");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error=%e, path=%dir.display(), "history dir create failed");
        return None;
    }
    Some(dir.join("system.jsonl"))
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

/// Append one sample. Rotates when the file exceeds the local peer's
/// resolved `max_mb` cap (or [`FALLBACK_MAX_BYTES`] when no override).
pub fn append(point: &SystemHistoryPoint) {
    let Some(path) = history_path() else {
        return;
    };
    let line = match serde_json::to_string(point) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error=%e, "history serialise failed");
            return;
        }
    };
    let mut f = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error=%e, path=%path.display(), "history open failed");
            return;
        }
    };
    if let Err(e) = writeln!(f, "{line}") {
        tracing::warn!(error=%e, "history write failed");
        return;
    }
    // Rotate if oversized: keep the second half of the file (drop oldest).
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if size > current_max_bytes() {
        rotate(&path, size);
    }
}

fn rotate(path: &std::path::Path, size: u64) {
    let keep_from = size / 2;
    let Ok(mut f) = File::open(path) else {
        return;
    };
    if f.seek(SeekFrom::Start(keep_from)).is_err() {
        return;
    }
    let reader = BufReader::new(&mut f);
    let mut kept: Vec<String> = Vec::new();
    let mut lines = reader.lines();
    // First line after seek is likely partial — drop it.
    let _ = lines.next();
    for line in lines.map_while(Result::ok) {
        if !line.is_empty() {
            kept.push(line);
        }
    }
    let tmp = path.with_extension("jsonl.tmp");
    let Ok(mut out) = File::create(&tmp) else {
        return;
    };
    for line in &kept {
        if writeln!(out, "{line}").is_err() {
            return;
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!(error=%e, "history rotate rename failed");
    }
}

/// Read the last `n` history points, filtered by the local peer's
/// resolved age cap (falls back to [`FALLBACK_MAX_AGE_SECS`]).
pub fn read_tail(n: usize) -> Vec<SystemHistoryPoint> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(f) = File::open(&path) else {
        return Vec::new();
    };
    let cutoff = chrono::Utc::now().timestamp() - current_max_age_secs();
    let reader = BufReader::new(f);
    // Simple tail: read all lines into a ring of size n. JSONL is small
    // enough (~5 MiB cap) that this is fine and avoids reverse-seek logic.
    let mut ring: std::collections::VecDeque<SystemHistoryPoint> =
        std::collections::VecDeque::with_capacity(n);
    for line in reader.lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(p) = serde_json::from_str::<SystemHistoryPoint>(&line) else {
            continue;
        };
        if p.ts < cutoff {
            continue;
        }
        if ring.len() == n {
            ring.pop_front();
        }
        ring.push_back(p);
    }
    ring.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_from_skips_empty_snapshot() {
        let snap = SystemInfoReport {
            snapshot_at_unix: Some(1),
            ..Default::default()
        };
        assert!(point_from(&snap).is_none());
    }

    #[test]
    fn point_from_keeps_with_cpu() {
        let snap = SystemInfoReport {
            snapshot_at_unix: Some(1),
            cpu_usage_percent: Some(12.0),
            ..Default::default()
        };
        let p = point_from(&snap).unwrap();
        assert_eq!(p.ts, 1);
        assert_eq!(p.cpu_percent, Some(12.0));
    }
}
