//! Network mount monitor (NFS + SMB/CIFS).
//!
//! Linux-only at runtime — relies on `/proc/mounts`, `stat`, and `umount`.
//! The parser is platform-agnostic so tests run on any OS.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;

const PROC_MOUNTS: &str = "/proc/mounts";

#[derive(Debug, Error)]
pub enum NfsError {
    #[error("read /proc/mounts: {0}")]
    Read(#[from] std::io::Error),
    #[error("umount -l {mountpoint}: {source}")]
    Umount {
        mountpoint: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseResult {
    pub released: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<ReleaseFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseFailure {
    pub mountpoint: String,
    pub error: String,
}

/// Network filesystem types this crate reports on.
fn is_network_fs(fstype: &str) -> bool {
    matches!(fstype, "nfs" | "nfs4" | "cifs" | "smbfs")
}

/// Read `/proc/mounts` into a typed list. Returns only network mounts.
pub fn read_mounts() -> Result<Vec<Mount>, NfsError> {
    let f = std::fs::File::open(PROC_MOUNTS)?;
    parse_mounts(f)
}

/// Parse a /proc/mounts-formatted stream. Pulled out for cross-platform tests.
pub fn parse_mounts<R: Read>(r: R) -> Result<Vec<Mount>, NfsError> {
    let mut out = Vec::new();
    for line in BufReader::new(r).lines() {
        let line = line?;
        let mut fields = line.split_whitespace();
        let (Some(device), Some(mountpoint), Some(fstype)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !is_network_fs(fstype) {
            continue;
        }
        out.push(Mount {
            device: device.to_string(),
            mountpoint: mountpoint.to_string(),
            fstype: fstype.to_string(),
            health: None,
        });
    }
    Ok(out)
}

/// Restrict mounts to a configured watch list. `/foo` matches `/foo` and
/// any subpath `/foo/...`. Empty watch list = pass through.
pub fn filter_watch(mounts: Vec<Mount>, watch: &[String]) -> Vec<Mount> {
    if watch.is_empty() {
        return mounts;
    }
    mounts
        .into_iter()
        .filter(|m| {
            watch
                .iter()
                .any(|w| match m.mountpoint.strip_prefix(w.as_str()) {
                    Some("") => true,
                    Some(rest) => rest.starts_with('/'),
                    None => false,
                })
        })
        .collect()
}

/// Filter by exact filesystem type. Empty filter = pass through.
pub fn filter_by_fstype(mounts: Vec<Mount>, fstype: &str) -> Vec<Mount> {
    if fstype.is_empty() {
        return mounts;
    }
    mounts.into_iter().filter(|m| m.fstype == fstype).collect()
}

/// `stat <mountpoint>` with a timeout. Returns `"ok"` / `"stale"` / `"error: …"`.
/// `stat` blocks in-kernel on stale NFS handles, so the timeout is the
/// only reliable detection signal.
pub async fn check_health(mountpoint: &str, timeout: Duration) -> String {
    let fut = Command::new("stat").arg("--").arg(mountpoint).output();
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => "stale".to_string(),
        Ok(Err(e)) => format!("error: {e}"),
        Ok(Ok(out)) if out.status.success() => "ok".to_string(),
        Ok(Ok(out)) => format!("error: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// `mounts.list` — read /proc/mounts, apply watch + type filters, probe health.
/// Health probes run concurrently so N stale mounts cost ~one timeout.
pub async fn list(
    watch: &[String],
    fstype_filter: &str,
    health_timeout: Duration,
) -> Result<Vec<Mount>, NfsError> {
    let mut mounts = filter_by_fstype(filter_watch(read_mounts()?, watch), fstype_filter);
    let probes: Vec<_> = mounts
        .iter()
        .map(|m| {
            let mp = m.mountpoint.clone();
            tokio::spawn(async move { check_health(&mp, health_timeout).await })
        })
        .collect();
    for (m, probe) in mounts.iter_mut().zip(probes) {
        m.health = Some(
            probe
                .await
                .unwrap_or_else(|e| format!("error: probe task failed: {e}")),
        );
    }
    Ok(mounts)
}

/// `mounts.release` — `umount -l` for matching mounts. Optional host substring
/// filter (matches against the device field, e.g. `10.10.10.10:/data`).
/// Failures are collected per-mount instead of fail-fast so partial success
/// is reported back; one stuck mount won't block the rest.
pub async fn release(host_filter: &str, fstype_filter: &str) -> Result<ReleaseResult, NfsError> {
    let mounts = filter_by_fstype(read_mounts()?, fstype_filter);
    let mut skipped = Vec::new();
    let mut targets = Vec::new();
    for m in mounts {
        if !host_filter.is_empty() && !m.device.contains(host_filter) {
            skipped.push(m.mountpoint);
        } else {
            targets.push(m.mountpoint);
        }
    }
    let attempts: Vec<_> = targets
        .into_iter()
        .map(|mp| {
            tokio::spawn(async move {
                let res = Command::new("umount").arg("-l").arg(&mp).status().await;
                (mp, res)
            })
        })
        .collect();
    let mut released = Vec::new();
    let mut failed = Vec::new();
    for handle in attempts {
        let (mp, res) = handle.await.map_err(|e| NfsError::Umount {
            mountpoint: "<unknown>".to_string(),
            source: std::io::Error::other(format!("join error: {e}")),
        })?;
        match res {
            Ok(status) if status.success() => released.push(mp),
            Ok(status) => failed.push(ReleaseFailure {
                mountpoint: mp,
                error: format!("exit code {status}"),
            }),
            Err(e) => failed.push(ReleaseFailure {
                mountpoint: mp,
                error: e.to_string(),
            }),
        }
    }
    Ok(ReleaseResult {
        released,
        skipped,
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
proc /proc proc rw,nosuid,nodev,noexec 0 0
10.10.10.10:/data /mnt/data nfs4 rw 0 0
//willow/share /mnt/willow cifs rw 0 0
/dev/sda1 / ext4 rw 0 0
malformed_line
nasbox:/legacy /mnt/legacy smbfs ro 0 0
";

    #[test]
    fn parse_filters_to_network_mounts() {
        let mounts = parse_mounts(SAMPLE.as_bytes()).unwrap();
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].fstype, "nfs4");
        assert_eq!(mounts[1].mountpoint, "/mnt/willow");
        assert_eq!(mounts[2].fstype, "smbfs");
    }

    #[test]
    fn filter_watch_restricts_to_listed_paths() {
        let mounts = parse_mounts(SAMPLE.as_bytes()).unwrap();
        let watch = vec!["/mnt/data".to_string()];
        let filtered = filter_watch(mounts, &watch);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].mountpoint, "/mnt/data");
    }

    #[test]
    fn filter_watch_matches_subpaths() {
        let mut mounts = parse_mounts(SAMPLE.as_bytes()).unwrap();
        mounts.push(Mount {
            device: "x".into(),
            mountpoint: "/mnt/data/sub".into(),
            fstype: "nfs".into(),
            health: None,
        });
        let watch = vec!["/mnt/data".to_string()];
        let filtered = filter_watch(mounts, &watch);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_watch_empty_passes_through() {
        let mounts = parse_mounts(SAMPLE.as_bytes()).unwrap();
        assert_eq!(filter_watch(mounts.clone(), &[]).len(), mounts.len());
    }

    #[test]
    fn filter_by_fstype_exact_match() {
        let mounts = parse_mounts(SAMPLE.as_bytes()).unwrap();
        let cifs_only = filter_by_fstype(mounts, "cifs");
        assert_eq!(cifs_only.len(), 1);
        assert_eq!(cifs_only[0].fstype, "cifs");
    }

    #[test]
    fn is_network_fs_recognises_kernel_clients() {
        assert!(is_network_fs("nfs"));
        assert!(is_network_fs("nfs4"));
        assert!(is_network_fs("cifs"));
        assert!(is_network_fs("smbfs"));
        assert!(!is_network_fs("ext4"));
        assert!(!is_network_fs("tmpfs"));
    }
}
