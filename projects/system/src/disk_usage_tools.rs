//! Top disk-space consumers under a path — the "which directories are huge"
//! companion to [`crate::capacity_diagnostics`]'s "is this filesystem
//! filling" trend. That provider answers *whether* a host is running out of
//! room; this verb answers *what's using it*, without requiring SSH or
//! `guest_exec` with raw `du`.
//!
//! Lists immediate children of a directory, each annotated with its total
//! recursive size, sorted largest first — the same `du -d 1 | sort -rh`
//! pattern useful for iterative drill-down: call again with the biggest
//! child as the new `path` to go one level deeper.
//!
//! Symlinks are never followed (avoids cycles and double-counting across
//! bind mounts). The scan is wall-clock bounded: a directory with millions
//! of files or a slow network mount returns a `truncated` partial result
//! instead of hanging indefinitely.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_LIMIT: usize = 20;
const MIN_LIMIT: usize = 1;
const MAX_LIMIT: usize = 200;

const DEFAULT_MAX_SECS: u64 = 20;
const MIN_MAX_SECS: u64 = 1;
const MAX_MAX_SECS: u64 = 120;

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DiskUsageArgs {
    /// Directory to break down. Defaults to the user's home directory.
    #[arg(long)]
    pub path: Option<String>,
    /// Max entries to return, sorted by size descending. Default 20,
    /// clamped to [1, 200].
    #[arg(long)]
    pub limit: Option<usize>,
    /// Scan time budget in seconds before returning a partial, truncated
    /// result. Default 20, clamped to [1, 120].
    #[arg(long = "max-secs")]
    pub max_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsageEntry {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub is_dir: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsageOutput {
    /// The resolved, canonicalized directory that was scanned.
    pub path: String,
    /// Immediate children of `path`, sorted by total size descending.
    pub entries: Vec<DiskUsageEntry>,
    /// Total capacity of the filesystem hosting `path`, when resolvable.
    pub filesystem_total_bytes: Option<u64>,
    /// Free space on the filesystem hosting `path`, when resolvable.
    pub filesystem_available_bytes: Option<u64>,
    /// True if the time budget was hit before every entry's size was fully
    /// computed — sizes for affected entries are a lower bound, not exact.
    pub truncated: bool,
}

/// Recursive size of `path`, never following symlinks. Best-effort: a
/// permission-denied or vanished entry contributes 0 rather than erroring
/// the whole scan. Stops early (marking `truncated`) once `deadline` passes.
fn dir_size(path: &Path, deadline: Instant, truncated: &mut bool) -> u64 {
    if Instant::now() >= deadline {
        *truncated = true;
        return 0;
    }
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            *truncated = true;
            break;
        }
        // `DirEntry::file_type` does not traverse a symlink target, so a
        // symlinked entry reports as a symlink here — explicitly skip it
        // rather than count or recurse into whatever it points at.
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            total = total.saturating_add(dir_size(&entry.path(), deadline, truncated));
        } else if let Ok(meta) = entry.metadata() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Core scan, independent of the tool-call plumbing so it's directly
/// unit-testable. `path` must already be resolved to a real directory.
fn breakdown(path: &Path, limit: usize, max_secs: u64) -> (Vec<DiskUsageEntry>, bool) {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    let mut truncated = false;
    let mut entries: Vec<DiskUsageEntry> = Vec::new();
    if let Ok(children) = std::fs::read_dir(path) {
        for entry in children.flatten() {
            if Instant::now() >= deadline {
                truncated = true;
                break;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_path = entry.path();
            let (bytes, is_dir) = if file_type.is_dir() {
                (dir_size(&child_path, deadline, &mut truncated), true)
            } else {
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                (len, false)
            };
            entries.push(DiskUsageEntry {
                name,
                path: child_path.to_string_lossy().into_owned(),
                bytes,
                is_dir,
            });
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    entries.truncate(limit);
    (entries, truncated)
}

/// Top disk-space consumers directly under `path`: the `du -d 1 | sort -rh`
/// pattern, one orca verb. Each entry's `bytes` is its full recursive size,
/// so drilling in is `disk_usage --path <biggest entry's path>` again.
#[orca_tool(domain = "system", verb = "disk_usage")]
async fn system_disk_usage(
    args: DiskUsageArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DiskUsageOutput> {
    let requested = match args.path {
        Some(p) => PathBuf::from(p),
        None => crate::install::home_dir()?,
    };
    let resolved = std::fs::canonicalize(&requested).unwrap_or(requested);
    let limit = args
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(MIN_LIMIT, MAX_LIMIT);
    let max_secs = args
        .max_secs
        .unwrap_or(DEFAULT_MAX_SECS)
        .clamp(MIN_MAX_SECS, MAX_MAX_SECS);

    let disk = crate::system_info::probe_disk(&resolved);
    let (entries, truncated) = breakdown(&resolved, limit, max_secs);

    Ok(DiskUsageOutput {
        path: resolved.to_string_lossy().into_owned(),
        entries,
        filesystem_total_bytes: disk.as_ref().map(|d| d.total),
        filesystem_available_bytes: disk.as_ref().map(|d| d.avail),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, bytes: usize) {
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a.bin"), 100);
        fs::create_dir(tmp.path().join("sub")).unwrap();
        write(&tmp.path().join("sub").join("b.bin"), 50);
        let mut truncated = false;
        let size = dir_size(
            tmp.path(),
            Instant::now() + Duration::from_secs(5),
            &mut truncated,
        );
        assert_eq!(size, 150);
        assert!(!truncated);
    }

    #[test]
    fn dir_size_never_follows_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("real")).unwrap();
        write(&tmp.path().join("real").join("big.bin"), 10_000);
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
        let mut truncated = false;
        let size = dir_size(
            tmp.path(),
            Instant::now() + Duration::from_secs(5),
            &mut truncated,
        );
        // Only "real" is counted once; the symlink to it contributes nothing.
        assert_eq!(size, 10_000);
    }

    #[test]
    fn dir_size_stops_at_an_already_passed_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a.bin"), 100);
        let mut truncated = false;
        let size = dir_size(
            tmp.path(),
            Instant::now() - Duration::from_secs(1),
            &mut truncated,
        );
        assert_eq!(size, 0);
        assert!(truncated);
    }

    #[test]
    fn breakdown_sorts_children_by_size_descending() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("big")).unwrap();
        write(&tmp.path().join("big").join("f.bin"), 1_000);
        fs::create_dir(tmp.path().join("small")).unwrap();
        write(&tmp.path().join("small").join("f.bin"), 10);
        write(&tmp.path().join("loose.bin"), 500);

        let (entries, truncated) = breakdown(tmp.path(), 20, 5);
        assert!(!truncated);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["big", "loose.bin", "small"]);
        assert_eq!(entries[0].bytes, 1_000);
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].bytes, 500);
        assert!(!entries[1].is_dir);
    }

    #[test]
    fn breakdown_respects_limit() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            write(&tmp.path().join(format!("f{i}.bin")), 10 + i);
        }
        let (entries, _) = breakdown(tmp.path(), 2, 5);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].bytes, 14);
        assert_eq!(entries[1].bytes, 13);
    }

    #[test]
    fn breakdown_skips_a_symlinked_child_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("real.bin"), 100);
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("real.bin"), tmp.path().join("link.bin"))
            .unwrap();
        let (entries, _) = breakdown(tmp.path(), 20, 5);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["real.bin"]);
    }

    #[test]
    fn args_deserialize_camel_case() {
        let a: DiskUsageArgs =
            serde_json::from_str(r#"{"path":"/tmp","limit":5,"maxSecs":10}"#).unwrap();
        assert_eq!(a.path.as_deref(), Some("/tmp"));
        assert_eq!(a.limit, Some(5));
        assert_eq!(a.max_secs, Some(10));
    }

    #[test]
    fn args_default_empty() {
        let a = DiskUsageArgs::default();
        assert!(a.path.is_none());
        assert!(a.limit.is_none());
        assert!(a.max_secs.is_none());
    }
}
