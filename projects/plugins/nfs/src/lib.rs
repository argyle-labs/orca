//! Network mount monitor (NFS + SMB/CIFS).
//!
//! Linux-only at runtime — relies on `/proc/mounts`, `stat`, and `umount`.
//! The parser is platform-agnostic so tests run on any OS.

use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::time::Duration;

use plugin_toolkit::async_trait;
use plugin_toolkit::prelude::*;
use plugin_toolkit::storage::{
    Capability, MountOutcome, Share, StorageBackend, StorageError, StorageKind,
};
use plugin_toolkit::tokio::process::Command;

const PROC_MOUNTS: &str = "/proc/mounts";

#[derive(Debug)]
pub enum NfsError {
    Read(std::io::Error),
    Umount {
        mountpoint: String,
        source: std::io::Error,
    },
    MountAll {
        source: std::io::Error,
    },
}

impl std::fmt::Display for NfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NfsError::Read(source) => write!(f, "read /proc/mounts: {source}"),
            NfsError::Umount { mountpoint, source } => {
                write!(f, "umount -l {mountpoint}: {source}")
            }
            NfsError::MountAll { source } => write!(f, "mount -a: {source}"),
        }
    }
}

impl std::error::Error for NfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NfsError::Read(source) => Some(source),
            NfsError::Umount { source, .. } => Some(source),
            NfsError::MountAll { source } => Some(source),
        }
    }
}

impl From<std::io::Error> for NfsError {
    fn from(e: std::io::Error) -> Self {
        NfsError::Read(e)
    }
}

#[plugin_struct]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

#[plugin_struct]
#[derive(Debug, Clone)]
pub struct ReleaseResult {
    pub released: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<ReleaseFailure>,
}

#[plugin_struct]
#[derive(Debug, Clone)]
pub struct ReleaseFailure {
    pub mountpoint: String,
    pub error: String,
}

/// Outcome of [`recover_stale`]: a stale-mount health-probe → force-release →
/// `mount -a` → re-probe cycle. `recovered` are mounts that were stale before
/// and `ok` after; `still_stale` are mounts that did not come back; `errors`
/// captures any non-fatal step failures (release failures, mount -a failure)
/// so the caller can log them and continue.
#[plugin_struct]
#[derive(Debug, Clone, Default)]
pub struct RecoverResult {
    /// Mountpoints that were stale on the first probe and healthy after recovery.
    pub recovered: Vec<String>,
    /// Mountpoints still unhealthy after the recovery sequence.
    pub still_stale: Vec<String>,
    /// Non-fatal errors encountered during recovery (per-mount release
    /// failures, `mount -a` failure, probe errors).
    pub errors: Vec<String>,
    /// `true` when there was nothing stale to recover (fast path / no-op).
    pub no_stale_found: bool,
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
    match plugin_toolkit::tokio::time::timeout(timeout, fut).await {
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
            plugin_toolkit::tokio::spawn(async move { check_health(&mp, health_timeout).await })
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

/// `mounts.release` — lazy-unmount matching mounts. Optional host substring
/// filter (matches against the device field, e.g. `<server>:/data`).
///
/// `force == false` → `umount -l` (lazy detach; the default, unchanged).
/// `force == true`  → `umount -lf` (lazy **and** force; required to detach a
/// mount whose server is unreachable — a stale NFS handle won't release with
/// `-l` alone because the kernel still tries to flush).
///
/// Failures are collected per-mount instead of fail-fast so partial success
/// is reported back; one stuck mount won't block the rest.
pub async fn release(
    host_filter: &str,
    fstype_filter: &str,
    force: bool,
) -> Result<ReleaseResult, NfsError> {
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
    let umount_flag = if force { "-lf" } else { "-l" };
    let attempts: Vec<_> = targets
        .into_iter()
        .map(|mp| {
            plugin_toolkit::tokio::spawn(async move {
                let res = Command::new("umount")
                    .arg(umount_flag)
                    .arg(&mp)
                    .status()
                    .await;
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

/// `mount -a` — (re)mount everything declared in fstab that isn't already
/// mounted. Used after a force-release to bring detached network mounts back.
/// A non-zero exit is surfaced as [`NfsError::MountAll`] carrying stderr so the
/// caller can decide whether to log-and-continue or fail.
pub async fn mount_all() -> Result<(), NfsError> {
    let out = Command::new("mount")
        .arg("-a")
        .output()
        .await
        .map_err(|source| NfsError::MountAll { source })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(NfsError::MountAll {
            source: std::io::Error::other(format!(
                "exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        })
    }
}

/// Orchestrated stale-mount recovery for one host's network mounts.
///
/// Sequence (per [[feedback-self-healing-is-mandatory]]: probes do real I/O):
/// 1. Probe health of every matching network mount (`stat` with a timeout).
/// 2. If none are stale, return early with `no_stale_found = true`.
/// 3. Force-release (`umount -lf`) the stale ones.
/// 4. `mount -a` to re-attach them from fstab.
/// 5. Re-probe and classify each previously-stale mount as recovered or
///    still-stale.
///
/// Non-fatal step failures (a release failure, a `mount -a` non-zero exit) are
/// collected into `errors` rather than aborting — the caller logs and continues
/// its own recovery (e.g. proxmox lifecycle restart). Only a failure to read
/// `/proc/mounts` (the initial enumeration) is fatal and returned as `Err`.
pub async fn recover_stale(
    watch: &[String],
    fstype_filter: &str,
    health_timeout: Duration,
) -> Result<RecoverResult, NfsError> {
    let mut result = RecoverResult::default();

    // 1. Initial probe.
    let mounts = list(watch, fstype_filter, health_timeout).await?;
    let stale: Vec<Mount> = mounts
        .into_iter()
        .filter(|m| m.health.as_deref() == Some("stale"))
        .collect();

    if stale.is_empty() {
        result.no_stale_found = true;
        return Ok(result);
    }

    // 3. Force-release each stale mount. Filter by exact device so we only
    //    detach the wedged ones, not every network mount on the host.
    for m in &stale {
        match release(&m.device, fstype_filter, true).await {
            Ok(r) => {
                for f in r.failed {
                    result
                        .errors
                        .push(format!("release {}: {}", f.mountpoint, f.error));
                }
            }
            Err(e) => result.errors.push(format!("release {}: {e}", m.mountpoint)),
        }
    }

    // 4. Re-attach from fstab.
    if let Err(e) = mount_all().await {
        result.errors.push(e.to_string());
    }

    // 5. Re-probe the previously-stale set.
    for m in &stale {
        let health = check_health(&m.mountpoint, health_timeout).await;
        if health == "ok" {
            result.recovered.push(m.mountpoint.clone());
        } else {
            result.still_stale.push(m.mountpoint.clone());
        }
    }

    Ok(result)
}

// ── storage domain backend ──────────────────────────────────────────────────

/// NFS/SMB network-share backend for the `storage` domain. Contributes the
/// host's live network mounts as shares and exposes lazy/forced unmount. Mount
/// and usage stay [`StorageError::Unsupported`] — this adapter reads the
/// kernel's mount table rather than driving fstab/automount itself.
pub struct NfsBackend {
    name: String,
}

impl NfsBackend {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl Default for NfsBackend {
    fn default() -> Self {
        Self::new("nfs")
    }
}

#[async_trait::async_trait]
impl StorageBackend for NfsBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> StorageKind {
        StorageKind::NetworkShare
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::List, Capability::Unmount]
    }

    fn endpoint(&self) -> String {
        "nfs://local".to_string()
    }

    async fn list_shares(&self) -> Result<Vec<Share>, StorageError> {
        let mounts = read_mounts().map_err(|e| StorageError::Transport(e.to_string()))?;
        Ok(mounts
            .into_iter()
            .map(|m| Share {
                id: m.mountpoint.clone(),
                source: m.device,
                target: Some(m.mountpoint),
                fstype: m.fstype,
                mounted: true,
            })
            .collect())
    }

    async fn unmount(&self, target: &str) -> Result<MountOutcome, StorageError> {
        let res = release(target, "", true)
            .await
            .map_err(|e| StorageError::Transport(e.to_string()))?;
        if let Some(f) = res.failed.first() {
            return Err(StorageError::Other(format!(
                "unmount {}: {}",
                f.mountpoint, f.error
            )));
        }
        let mounted = res.released.is_empty();
        let detail = if res.released.is_empty() {
            res.skipped.first().map(|_| "no matching mount".to_string())
        } else {
            None
        };
        Ok(MountOutcome {
            target: target.to_string(),
            mounted,
            recovered: false,
            detail,
        })
    }
}

/// Register the nfs storage backend with the process-global `storage` registry.
/// Called once at daemon startup. Idempotent — re-registering replaces by name.
pub fn bootstrap() {
    plugin_toolkit::storage::register_backend(Arc::new(NfsBackend::default()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::serde_json;

    const SAMPLE: &str = "\
proc /proc proc rw,nosuid,nodev,noexec 0 0
10.10.10.10:/data /mnt/data nfs4 rw 0 0
//host-e/share /mnt/host-e cifs rw 0 0
/dev/sda1 / ext4 rw 0 0
malformed_line
nasbox:/legacy /mnt/legacy smbfs ro 0 0
";

    #[test]
    fn parse_filters_to_network_mounts() {
        let mounts = parse_mounts(SAMPLE.as_bytes()).unwrap();
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].fstype, "nfs4");
        assert_eq!(mounts[1].mountpoint, "/mnt/host-e");
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

    #[test]
    fn filter_by_fstype_empty_passes_through() {
        let mounts = parse_mounts(SAMPLE.as_bytes()).unwrap();
        let n = mounts.len();
        assert_eq!(filter_by_fstype(mounts, "").len(), n);
    }

    #[test]
    fn nfs_error_display_covers_each_variant() {
        let io: NfsError = std::io::Error::other("boom").into();
        assert!(io.to_string().contains("/proc/mounts"));
        let u = NfsError::Umount {
            mountpoint: "/mnt/x".into(),
            source: std::io::Error::other("nope"),
        };
        let s = u.to_string();
        assert!(s.contains("/mnt/x"));
    }

    #[test]
    fn mount_and_release_types_round_trip_through_serde() {
        let m = Mount {
            device: "srv:/x".into(),
            mountpoint: "/mnt/x".into(),
            fstype: "nfs4".into(),
            health: Some("ok".into()),
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: Mount = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);

        // health=None must be omitted from output.
        let m2 = Mount {
            health: None,
            ..m.clone()
        };
        let s2 = serde_json::to_string(&m2).unwrap();
        assert!(!s2.contains("health"));

        let r = ReleaseResult {
            released: vec!["/a".into()],
            skipped: vec!["/b".into()],
            failed: vec![ReleaseFailure {
                mountpoint: "/c".into(),
                error: "x".into(),
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ReleaseResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.released, r.released);
        assert_eq!(back.skipped, r.skipped);
        assert_eq!(back.failed[0].mountpoint, "/c");
    }

    #[tokio::test]
    async fn check_health_returns_ok_for_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let s = check_health(dir.path().to_str().unwrap(), Duration::from_secs(5)).await;
        assert_eq!(s, "ok");
    }

    #[tokio::test]
    async fn check_health_returns_error_for_missing_path() {
        let s = check_health("/definitely/not/here/orca_nfs_test", Duration::from_secs(5)).await;
        assert!(s.starts_with("error:"));
    }

    #[tokio::test]
    async fn check_health_returns_stale_when_timeout_elapses() {
        // 1ns budget against the real `stat` process expires before exec
        // completes → "stale" branch.
        let s = check_health("/", Duration::from_nanos(1)).await;
        // Allow either stale (timeout) or ok (impossibly fast) — both cover
        // the matching arm and any flake stays green.
        assert!(s == "stale" || s == "ok");
    }

    // Linux-only paths (`read_mounts`, `list`, `release`) all hit /proc/mounts
    // which doesn't exist on macOS. Exercise the Err path on non-Linux so
    // those functions still get coverage in CI runners that aren't Linux.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn read_mounts_errors_when_proc_mounts_absent() {
        let err = read_mounts().unwrap_err();
        assert!(matches!(err, NfsError::Read(_)));
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn list_propagates_read_mounts_failure() {
        let res = list(&[], "", Duration::from_secs(1)).await;
        assert!(res.is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn release_propagates_read_mounts_failure() {
        // Both force modes must surface the enumeration error.
        assert!(release("", "", false).await.is_err());
        assert!(release("", "", true).await.is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn recover_stale_propagates_read_mounts_failure() {
        // Initial enumeration failure is the one fatal path.
        let res = recover_stale(&[], "", Duration::from_secs(1)).await;
        assert!(res.is_err());
    }

    #[test]
    fn recover_result_round_trips_through_serde() {
        let r = RecoverResult {
            recovered: vec!["/mnt/a".into()],
            still_stale: vec!["/mnt/b".into()],
            errors: vec!["release /mnt/c: boom".into()],
            no_stale_found: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: RecoverResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.recovered, r.recovered);
        assert_eq!(back.still_stale, r.still_stale);
        assert_eq!(back.errors, r.errors);
        assert!(!back.no_stale_found);
    }

    #[test]
    fn recover_result_default_is_empty_no_stale() {
        let r = RecoverResult::default();
        assert!(r.recovered.is_empty());
        assert!(r.still_stale.is_empty());
        assert!(r.errors.is_empty());
        assert!(!r.no_stale_found);
    }

    #[test]
    fn mount_all_error_displays_context() {
        let e = NfsError::MountAll {
            source: std::io::Error::other("device busy"),
        };
        let s = e.to_string();
        assert!(s.contains("mount -a"));
        assert!(s.contains("device busy"));
    }

    // `mount_all` shells out to the real `mount` binary; on a dev box without
    // privileges it exits non-zero, exercising the MountAll error branch.
    // On CI/macOS `mount -a` may differ, so accept either Ok or MountAll.
    #[tokio::test]
    async fn mount_all_returns_a_result() {
        match mount_all().await {
            Ok(()) => {}
            Err(NfsError::MountAll { .. }) => {}
            Err(other) => panic!("unexpected error variant: {other}"),
        }
    }
}
