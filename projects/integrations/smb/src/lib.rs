//! SMB / CIFS integration. Mirrors the shape of the nfs module but for
//! SMB-mounted shares. Linux uses `mount.cifs` / `umount`; macOS uses
//! `mount_smbfs`. Discovery (list shares on a server) goes through
//! `smbclient -L` on platforms where it's installed.
//!
//! This module shells out — there is no quality cross-platform Rust SMB
//! client crate that handles the kernel-mount and userspace-share-listing
//! cases together. Shelling out also means the user's existing kerberos
//! / smb.conf / cifs creds files keep working.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Error)]
pub enum SmbError {
    #[error("required tool not found on PATH: {0}")]
    MissingTool(&'static str),
    #[error("smb tool failed: {tool} (exit {code:?}): {stderr}")]
    ToolFailed {
        tool: &'static str,
        code: Option<i32>,
        stderr: String,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("operation timed out after {0:?}")]
    Timeout(Duration),
    #[error("unsupported on this platform")]
    Unsupported,
}

/// One mounted SMB/CIFS share, parsed from `/proc/mounts` (Linux) or
/// `mount` (macOS).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mount {
    /// Source — `//server/share` on Linux, `//user@server/share` on macOS.
    pub source: String,
    pub mountpoint: PathBuf,
    pub fs_type: String,
    pub options: Vec<String>,
}

/// One share advertised by a server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Share {
    pub name: String,
    pub kind: ShareKind,
    pub comment: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShareKind {
    Disk,
    Ipc,
    Printer,
    Other,
}

/// Health probe outcome. Mirrors the nfs module so the two can be combined
/// into a single dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Ok,
    Stale,
    Missing,
    Timeout,
    Error,
}

/// Credentials for [`mount`]. Either a creds-file path (with `username=` and
/// `password=` lines, as cifs.upcall expects) or inline username+password.
#[derive(Debug, Clone)]
pub enum Credentials {
    File(PathBuf),
    Inline { username: String, password: String },
    Guest,
}

#[derive(Debug, Clone)]
pub struct MountSpec<'a> {
    pub server: &'a str,
    pub share: &'a str,
    pub mountpoint: &'a Path,
    pub credentials: Credentials,
    /// Extra CIFS options passed via `-o`. Typical: `vers=3.0`, `iocharset=utf8`,
    /// `uid=1000`, `noperm`. Server/share/creds are inserted alongside.
    pub extra_opts: Vec<String>,
}

/// List currently-mounted SMB/CIFS shares.
#[cfg(target_os = "linux")]
pub async fn list_mounts() -> Result<Vec<Mount>, SmbError> {
    let raw = tokio::fs::read_to_string("/proc/mounts").await?;
    Ok(parse_proc_mounts(&raw))
}

/// macOS equivalent — `/sbin/mount` listing.
#[cfg(target_os = "macos")]
pub async fn list_mounts() -> Result<Vec<Mount>, SmbError> {
    let output = Command::new("/sbin/mount").output().await?;
    if !output.status.success() {
        return Err(SmbError::ToolFailed {
            tool: "mount",
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(parse_macos_mounts(
        std::str::from_utf8(&output.stdout).unwrap_or(""),
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub async fn list_mounts() -> Result<Vec<Mount>, SmbError> {
    Err(SmbError::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn parse_proc_mounts(raw: &str) -> Vec<Mount> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let source = parts.next()?;
            let mountpoint = parts.next()?;
            let fs_type = parts.next()?;
            let opts = parts.next()?;
            if !matches!(fs_type, "cifs" | "smb3" | "smbfs") {
                return None;
            }
            Some(Mount {
                source: unescape_octal(source),
                mountpoint: PathBuf::from(unescape_octal(mountpoint)),
                fs_type: fs_type.to_string(),
                options: opts.split(',').map(|s| s.to_string()).collect(),
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub(crate) fn parse_macos_mounts(raw: &str) -> Vec<Mount> {
    // Sample line:
    //   //user@server/share on /Volumes/share (smbfs, nodev, nosuid, mounted by user)
    raw.lines()
        .filter_map(|line| {
            let (source, rest) = line.split_once(" on ")?;
            let (mountpoint, opts) = rest.split_once(" (")?;
            let opts = opts.trim_end_matches(')');
            let mut parts = opts.split(',').map(|s| s.trim());
            let fs_type = parts.next()?.to_string();
            if !matches!(fs_type.as_str(), "smbfs" | "cifs") {
                return None;
            }
            let options: Vec<String> = parts.map(|s| s.to_string()).collect();
            Some(Mount {
                source: source.to_string(),
                mountpoint: PathBuf::from(mountpoint),
                fs_type,
                options,
            })
        })
        .collect()
}

/// `/proc/mounts` octal-escapes spaces, tabs, and a few specials. Reverse it.
#[cfg(target_os = "linux")]
fn unescape_octal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let mut digits = String::with_capacity(3);
            for _ in 0..3 {
                match chars.peek() {
                    Some(d) if d.is_ascii_digit() => digits.push(chars.next().unwrap()),
                    _ => break,
                }
            }
            if digits.len() == 3 {
                if let Ok(n) = u8::from_str_radix(&digits, 8) {
                    out.push(n as char);
                    continue;
                }
            }
            out.push('\\');
            out.push_str(&digits);
        } else {
            out.push(c);
        }
    }
    out
}

/// Quick health probe of a mountpoint. `stat` against the path, time-bound
/// so a hung mount can't block the caller.
pub async fn health(mountpoint: &Path, probe_timeout: Duration) -> Health {
    if !mountpoint.exists() {
        return Health::Missing;
    }
    let path = mountpoint.to_path_buf();
    let probe = tokio::task::spawn_blocking(move || std::fs::metadata(&path));
    match timeout(probe_timeout, probe).await {
        Err(_) => Health::Timeout,
        Ok(Ok(Ok(_))) => Health::Ok,
        Ok(Ok(Err(e))) if e.kind() == std::io::ErrorKind::NotFound => Health::Missing,
        Ok(Ok(Err(_))) => Health::Stale,
        Ok(Err(_)) => Health::Error,
    }
}

/// Mount an SMB share. Linux uses `mount.cifs`; macOS uses `mount_smbfs`.
/// Caller must have permission to mount (typically root on Linux, current
/// user on macOS).
pub async fn mount(spec: MountSpec<'_>) -> Result<(), SmbError> {
    #[cfg(target_os = "linux")]
    {
        which_or_err("mount.cifs").await?;
        let mut opts: Vec<String> = Vec::new();
        match &spec.credentials {
            Credentials::File(p) => opts.push(format!("credentials={}", p.display())),
            Credentials::Inline { username, password } => {
                opts.push(format!("username={username}"));
                opts.push(format!("password={password}"));
            }
            Credentials::Guest => opts.push("guest".to_string()),
        }
        opts.extend(spec.extra_opts.iter().cloned());
        let source = format!("//{}/{}", spec.server, spec.share);
        run_tool(
            "mount.cifs",
            &[
                source.as_str(),
                spec.mountpoint.to_str().unwrap_or(""),
                "-o",
                opts.join(",").as_str(),
            ],
        )
        .await
    }
    #[cfg(target_os = "macos")]
    {
        which_or_err("mount_smbfs").await?;
        let auth_part = match &spec.credentials {
            Credentials::Inline { username, password } => {
                format!("{}:{}@", urlencode(username), urlencode(password))
            }
            Credentials::Guest => String::new(),
            Credentials::File(_) => String::new(), // macOS uses keychain; ignored.
        };
        let url = format!("//{}{}/{}", auth_part, spec.server, spec.share);
        run_tool(
            "mount_smbfs",
            &[url.as_str(), spec.mountpoint.to_str().unwrap_or("")],
        )
        .await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = spec;
        Err(SmbError::Unsupported)
    }
}

/// Unmount a previously-mounted share.
pub async fn unmount(mountpoint: &Path) -> Result<(), SmbError> {
    which_or_err("umount").await?;
    run_tool("umount", &[mountpoint.to_str().unwrap_or("")]).await
}

/// List shares advertised by `server` via `smbclient -L //server`.
pub async fn list_shares(server: &str, credentials: &Credentials) -> Result<Vec<Share>, SmbError> {
    which_or_err("smbclient").await?;
    let mut args: Vec<String> = vec![format!("-L"), format!("//{server}"), "-g".into()];
    match credentials {
        Credentials::Guest => args.push("-N".into()),
        Credentials::Inline { username, password } => {
            args.push("-U".into());
            args.push(format!("{username}%{password}"));
        }
        Credentials::File(p) => {
            args.push("-A".into());
            args.push(p.display().to_string());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = Command::new("smbclient").args(&arg_refs).output().await?;
    if !output.status.success() {
        return Err(SmbError::ToolFailed {
            tool: "smbclient",
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(parse_smbclient_shares(
        std::str::from_utf8(&output.stdout).unwrap_or(""),
    ))
}

pub(crate) fn parse_smbclient_shares(raw: &str) -> Vec<Share> {
    // -g (grep-friendly) format: lines like
    //   Disk|public|Public files
    //   IPC|IPC$|IPC Service (Samba 4.x)
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split('|');
            let kind = parts.next()?;
            let name = parts.next()?;
            let comment = parts.next().unwrap_or("");
            let kind = match kind.trim() {
                "Disk" => ShareKind::Disk,
                "IPC" => ShareKind::Ipc,
                "Printer" => ShareKind::Printer,
                _ => return None,
            };
            Some(Share {
                name: name.to_string(),
                kind,
                comment: comment.to_string(),
            })
        })
        .collect()
}

async fn which_or_err(tool: &'static str) -> Result<(), SmbError> {
    let res = Command::new("which").arg(tool).output().await?;
    if res.status.success() {
        Ok(())
    } else {
        Err(SmbError::MissingTool(tool))
    }
}

async fn run_tool(tool: &'static str, args: &[&str]) -> Result<(), SmbError> {
    let out = Command::new(tool).args(args).output().await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(SmbError::ToolFailed {
            tool,
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(target_os = "macos")]
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            for byte in c.to_string().as_bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_mounts_picks_cifs_lines() {
        let sample = "\
//srv/public /mnt/public cifs ro,relatime,vers=3.0 0 0
tmpfs /run tmpfs rw,nosuid 0 0
//srv/backup /mnt/backup smb3 rw,vers=3.1.1 0 0
/dev/nvme0n1 / ext4 rw 0 0
";
        let mounts = parse_proc_mounts(sample);
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].fs_type, "cifs");
        assert_eq!(mounts[0].mountpoint, PathBuf::from("/mnt/public"));
        assert!(mounts[0].options.contains(&"ro".to_string()));
        assert_eq!(mounts[1].fs_type, "smb3");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unescape_octal_handles_spaces_and_tabs() {
        assert_eq!(unescape_octal("/mnt/has\\040space"), "/mnt/has space");
        assert_eq!(unescape_octal("/mnt/plain"), "/mnt/plain");
    }

    #[test]
    fn parse_smbclient_shares_extracts_disk_and_ipc() {
        let raw = "\
Disk|public|Public files
Disk|backup|
IPC|IPC$|IPC Service
Printer|hpoffice|HP printer
something invalid
";
        let shares = parse_smbclient_shares(raw);
        assert_eq!(shares.len(), 4);
        assert_eq!(shares[0].kind, ShareKind::Disk);
        assert_eq!(shares[0].name, "public");
        assert_eq!(shares[2].kind, ShareKind::Ipc);
        assert_eq!(shares[3].kind, ShareKind::Printer);
    }

    #[tokio::test]
    async fn health_missing_when_path_absent() {
        let h = health(
            Path::new("/nonexistent_orca_smb_test"),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(h, Health::Missing);
    }

    #[tokio::test]
    async fn health_ok_for_real_dir() {
        let dir = tempfile::tempdir().unwrap();
        let h = health(dir.path(), Duration::from_secs(1)).await;
        assert_eq!(h, Health::Ok);
    }
}
