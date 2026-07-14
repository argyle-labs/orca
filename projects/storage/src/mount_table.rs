//! Cross-platform mount-table primitive shared by every network-share backend.
//!
//! Reading the kernel mount table is OS-specific (`/proc/mounts` on Linux,
//! `/sbin/mount` output on macOS) and was previously duplicated — and divergent
//! — across the `nfs` and `smb` plugins. This module is the single source: a
//! typed [`MountEntry`], a typed [`Health`], the platform-gated [`mount_table`]
//! reader, and a runtime-agnostic timed [`probe_health`]. Backends filter the
//! table by fstype and contribute the rows as `storage` shares.
//!
//! Kept synchronous (std only) so the `storage` domain stays tokio-free; async
//! callers wrap [`probe_health`] in `spawn_blocking` if they must.

use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One row of the kernel mount table, normalized across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MountEntry {
    /// Mount source as the OS reports it: `host:/export` (NFS),
    /// `//server/share` (SMB), a device node, etc.
    pub source: String,
    /// Absolute mountpoint path.
    pub mountpoint: String,
    /// Filesystem / transport type (`nfs4`, `cifs`, `smbfs`, `apfs`, …).
    pub fstype: String,
    /// Mount options as individual tokens (`rw`, `vers=4.2`, `nosuid`, …).
    #[serde(default)]
    pub options: Vec<String>,
}

/// Liveness classification for a mountpoint. Shared so the nfs/smb dashboards
/// speak one language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Path is a live mount and answers I/O within the budget.
    Ok,
    /// Mount is present but I/O hung past the timeout (unreachable server).
    Stale,
    /// Path does not exist / nothing mounted there.
    Missing,
    /// Probe exceeded its time budget without a definite stale/ok answer.
    Timeout,
    /// Probe failed for some other reason.
    Error,
}

/// Read the live kernel mount table for the current platform. Unsupported
/// platforms return an empty table rather than erroring so callers degrade
/// gracefully.
pub fn mount_table() -> std::io::Result<Vec<MountEntry>> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string("/proc/mounts")?;
        Ok(parse_linux_proc_mounts(&raw))
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/sbin/mount").output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "/sbin/mount exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(parse_macos_mount(&String::from_utf8_lossy(&out.stdout)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Ok(Vec::new())
    }
}

/// The live mount table restricted to a set of filesystem types. Empty filter
/// returns everything.
pub fn mount_table_of(fstypes: &[&str]) -> std::io::Result<Vec<MountEntry>> {
    let all = mount_table()?;
    if fstypes.is_empty() {
        return Ok(all);
    }
    Ok(all
        .into_iter()
        .filter(|m| fstypes.contains(&m.fstype.as_str()))
        .collect())
}

/// Parse a `/proc/mounts`-formatted stream (Linux). Pure so tests run anywhere.
pub fn parse_linux_proc_mounts(raw: &str) -> Vec<MountEntry> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let source = parts.next()?;
            let mountpoint = parts.next()?;
            let fstype = parts.next()?;
            let opts = parts.next().unwrap_or("");
            Some(MountEntry {
                source: unescape_octal(source),
                mountpoint: unescape_octal(mountpoint),
                fstype: fstype.to_string(),
                options: opts.split(',').map(|s| s.to_string()).collect(),
            })
        })
        .collect()
}

/// Parse `/sbin/mount` output (macOS / BSD). Lines look like:
/// `//user@srv/share on /Volumes/share (smbfs, nodev, nosuid, mounted by u)`.
/// Pure so tests run on any platform.
pub fn parse_macos_mount(raw: &str) -> Vec<MountEntry> {
    raw.lines()
        .filter_map(|line| {
            let (source, rest) = line.split_once(" on ")?;
            let (mountpoint, opts) = rest.split_once(" (")?;
            let opts = opts.trim_end_matches(')');
            let mut parts = opts.split(',').map(|s| s.trim());
            let fstype = parts.next()?.to_string();
            let options: Vec<String> = parts.map(|s| s.to_string()).collect();
            Some(MountEntry {
                source: source.to_string(),
                mountpoint: mountpoint.to_string(),
                fstype,
                options,
            })
        })
        .collect()
}

/// `/proc/mounts` octal-escapes spaces, tabs, and a few specials. Reverse it.
fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut digits = String::with_capacity(3);
        for _ in 0..3 {
            match chars.peek() {
                Some(d) if d.is_ascii_digit() => digits.push(chars.next().unwrap()),
                _ => break,
            }
        }
        if digits.len() == 3
            && let Ok(n) = u8::from_str_radix(&digits, 8)
        {
            out.push(n as char);
        } else {
            out.push('\\');
            out.push_str(&digits);
        }
    }
    out
}

/// Time-bounded liveness probe of a mountpoint. Runtime-agnostic: the blocking
/// `stat` runs on a worker thread and the result is awaited with a timeout, so a
/// hung (stale) NFS/SMB handle classifies as [`Health::Stale`] instead of
/// blocking the caller forever. Async callers should still wrap this in
/// `spawn_blocking` since it parks a thread for up to `timeout`.
pub fn probe_health(mountpoint: &str, timeout: Duration) -> Health {
    let path = Path::new(mountpoint);
    if !path.exists() {
        return Health::Missing;
    }
    let owned = mountpoint.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    // Detached worker: if it blocks on a stale handle it leaks one thread until
    // the kernel gives up — acceptable and unavoidable for stale NFS.
    std::thread::spawn(move || {
        drop(tx.send(std::fs::metadata(&owned).map(|_| ())));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => Health::Ok,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Health::Missing,
        Ok(Err(_)) => Health::Stale,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Health::Stale,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Health::Error,
    }
}

/// Parse a mount `source` into the `(host, port)` of the server backing it, so
/// callers can run a TCP liveness probe against the transport itself. Backend-
/// agnostic via `fstype` so new share types plug in here (NFS today, SMB next):
///
/// - **NFS** (`fstype` starts with `nfs`, e.g. `nfs`/`nfs4`): `source` is
///   `host:/export`; the host is everything before the first `:` and the port is
///   the NFS port `2049`. Returns `None` if there is no `:`.
/// - **SMB/CIFS** (`fstype` is `cifs` or `smbfs`): `source` is `//server/share`
///   (optionally `//user@server/share`); the authority is taken up to the next
///   `/`, any `user@` prefix stripped, and the port is the SMB port `445`.
///   Returns `None` if `source` does not start with `//`.
/// - Any other `fstype`: `None` — not a network source we probe.
///
/// The host is trimmed of surrounding whitespace.
pub fn source_endpoint(source: &str, fstype: &str) -> Option<(String, u16)> {
    if fstype.starts_with("nfs") {
        let (host, _export) = source.split_once(':')?;
        let host = host.trim();
        if host.is_empty() {
            return None;
        }
        Some((host.to_string(), 2049))
    } else if fstype == "cifs" || fstype == "smbfs" {
        let rest = source.strip_prefix("//")?;
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or("").trim();
        if host.is_empty() {
            return None;
        }
        Some((host.to_string(), 445))
    } else {
        None
    }
}

/// Real transport liveness probe: resolve the server behind a mount `source` and
/// attempt a bounded TCP connect, returning `true` on the first reachable
/// address. Replaces stat-on-trigger-dir checks that false-positive on an
/// autofs mountpoint whose server is actually down.
///
/// Returns `false` when the source is not a network source ([`source_endpoint`]
/// yields `None`), when DNS resolution fails, or when every resolved address
/// fails to connect within `timeout`. Kept synchronous/std-only so the `storage`
/// domain stays tokio-free; async callers wrap it in `spawn_blocking`, mirroring
/// [`probe_health`].
pub fn probe_source(source: &str, fstype: &str, timeout: Duration) -> bool {
    use std::net::ToSocketAddrs;
    let Some((host, port)) = source_endpoint(source, fstype) else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_linux_proc_mounts_normalizes_rows() {
        let raw = "\
192.0.2.10:/srv/pool/data /mnt/pool/data nfs4 rw,vers=4.2 0 0
//srv/public /mnt/public cifs ro,relatime 0 0
/dev/sda1 / ext4 rw 0 0
malformed
";
        let m = parse_linux_proc_mounts(raw);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].fstype, "nfs4");
        assert_eq!(m[0].mountpoint, "/mnt/pool/data");
        assert!(m[0].options.contains(&"vers=4.2".to_string()));
        assert_eq!(m[1].source, "//srv/public");
    }

    #[test]
    fn parse_linux_unescapes_spaces() {
        let raw = "srv:/x /mnt/has\\040space nfs4 rw 0 0\n";
        let m = parse_linux_proc_mounts(raw);
        assert_eq!(m[0].mountpoint, "/mnt/has space");
    }

    #[test]
    fn parse_macos_mount_normalizes_rows() {
        let raw = "\
//user@srv/public on /Volumes/public (smbfs, nodev, nosuid, mounted by user)
/dev/disk1s1 on / (apfs, local, journaled)
10.0.0.5:/export on /Volumes/nfs (nfs)
no parens line
";
        let m = parse_macos_mount(raw);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].fstype, "smbfs");
        assert_eq!(m[0].source, "//user@srv/public");
        assert_eq!(m[0].mountpoint, "/Volumes/public");
        assert!(m[0].options.contains(&"nodev".to_string()));
        assert_eq!(m[1].fstype, "apfs");
        assert_eq!(m[2].fstype, "nfs");
    }

    #[test]
    fn fstype_filter_restricts() {
        let raw = "a:/x /mnt/x nfs4 rw 0 0\n//b/y /mnt/y cifs rw 0 0\n/dev/z / ext4 rw 0 0\n";
        let all = parse_linux_proc_mounts(raw);
        let net: Vec<_> = all
            .into_iter()
            .filter(|m| ["nfs4", "cifs"].contains(&m.fstype.as_str()))
            .collect();
        assert_eq!(net.len(), 2);
    }

    #[test]
    fn probe_health_missing_for_absent_path() {
        assert_eq!(
            probe_health("/nonexistent_orca_storage_probe", Duration::from_secs(1)),
            Health::Missing
        );
    }

    #[test]
    fn probe_health_ok_for_real_dir() {
        let dir = std::env::temp_dir();
        assert_eq!(
            probe_health(dir.to_str().unwrap(), Duration::from_secs(2)),
            Health::Ok
        );
    }

    #[test]
    fn health_round_trips_through_serde() {
        for h in [
            Health::Ok,
            Health::Stale,
            Health::Missing,
            Health::Timeout,
            Health::Error,
        ] {
            let j = serde_json::to_string(&h).unwrap();
            let back: Health = serde_json::from_str(&j).unwrap();
            assert_eq!(back, h);
        }
    }

    #[test]
    fn source_endpoint_parses_nfs() {
        assert_eq!(
            source_endpoint("primary:/srv/pool/data", "nfs4"),
            Some(("primary".to_string(), 2049))
        );
        assert_eq!(
            source_endpoint("10.0.0.5:/export", "nfs"),
            Some(("10.0.0.5".to_string(), 2049))
        );
    }

    #[test]
    fn source_endpoint_nfs_without_colon_is_none() {
        assert_eq!(source_endpoint("primary", "nfs4"), None);
    }

    #[test]
    fn source_endpoint_parses_smb() {
        assert_eq!(
            source_endpoint("//server/share", "cifs"),
            Some(("server".to_string(), 445))
        );
        assert_eq!(
            source_endpoint("//user@server/share", "cifs"),
            Some(("server".to_string(), 445))
        );
        assert_eq!(
            source_endpoint("//user@server/share", "smbfs"),
            Some(("server".to_string(), 445))
        );
    }

    #[test]
    fn source_endpoint_disk_fstype_is_none() {
        assert_eq!(source_endpoint("/dev/sda1", "ext4"), None);
    }

    #[test]
    fn source_endpoint_trims_whitespace() {
        assert_eq!(
            source_endpoint("  primary  :/srv/pool", "nfs4"),
            Some(("primary".to_string(), 2049))
        );
    }

    #[test]
    fn probe_source_unroutable_is_false() {
        // TEST-NET-1 (192.0.2.0/24) is reserved and never routable, so a tight
        // 1ms budget makes this deterministic and fast.
        assert!(!probe_source(
            "192.0.2.1:/export",
            "nfs4",
            Duration::from_millis(1)
        ));
    }

    #[test]
    fn mount_entry_round_trips_through_serde() {
        let e = MountEntry {
            source: "srv:/x".into(),
            mountpoint: "/mnt/x".into(),
            fstype: "nfs4".into(),
            options: vec!["rw".into(), "vers=4.2".into()],
        };
        let j = serde_json::to_string(&e).unwrap();
        let back: MountEntry = serde_json::from_str(&j).unwrap();
        assert_eq!(back, e);
    }
}
