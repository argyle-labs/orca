//! Generic native mount executor — the mechanism that replaces autofs.
//!
//! orca owns the mount lifecycle directly: it invokes the host's native
//! `mount(8)` / `umount(8)` with a fully-rendered spec and lets the kernel's own
//! mount helper (`mount.nfs`, `mount.cifs`, …) do the protocol work. There is no
//! automounter, no map file, and no protocol-specific code here — a mount is
//! `(source, target, fstype, options)`, all four supplied by the caller. The
//! backend plugin (`argyle-labs/nfs`) renders `fstype` + `options`; core just
//! runs the command.
//!
//! Why exec the native binary rather than `mount(2)` directly: the kernel NFS
//! path still wants a resolved server address + negotiated version in its mount
//! data, which `mount.nfs` builds. Exec-ing `mount` is portable across the
//! fleet's Linux hosts (Proxmox/Debian, Unraid/Slackware), uses the OS's tested
//! helper, and is trivially loggable. A pure `nix::mount(2)` applier is a viable
//! swap later — the argv construction below is the only concrete contract, and
//! it is isolated so the executor can change without touching callers.
//!
//! Runs **root-side only**, inside the `orca admin storage-apply` helper behind
//! the existing `sudo -n` privilege boundary — the daemon never mounts directly.

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// A root-owned secret file the owning backend needs materialized on the host
/// before the mount runs — an SMB `credentials=<path>` file, say. Generic seam:
/// the plugin resolves its own `SecretRef` and renders `contents`; core writes
/// the bytes to `path` (mode `0600`, path validated against the secret-file
/// allowlist) and reaps it on teardown, never knowing the grammar. `path` must be
/// a legal secret-file path under `SECRET_FILE_DIR` (see
/// [`plugin_toolkit::storage::is_valid_secret_file_path`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretFile {
    pub path: String,
    pub contents: String,
}

/// A single mount to realize on this host. Every field is already rendered by
/// the owning backend; the executor interprets none of them beyond passing them
/// to `mount(8)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountReq {
    /// Source as the kernel mount helper expects it (`host:/export` for NFS).
    /// For a mount with ordered failover sources this is the single elected
    /// source — orca owns source election, one source per attempt.
    pub source: String,
    /// Absolute mountpoint.
    pub target: String,
    /// Filesystem / transport type passed as `-t` (`nfs4`, `cifs`, …).
    pub fstype: String,
    /// Comma-joined option string passed as `-o`. Empty = no `-o` flag.
    pub options: String,
    /// Optional root-owned secret-file the backend produced (inline-SMB creds).
    /// Core writes it 0600 before mounting; `None` for NFS and file/guest-SMB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_file: Option<SecretFile>,
}

/// Build the `mount(8)` argument vector for `req` (everything after the program
/// name). `--` terminates option parsing so a source/target starting with `-`
/// can never be read as a flag. An empty option string omits `-o` entirely
/// rather than passing `-o ""` (which some `mount` builds reject).
pub fn mount_argv(req: &MountReq) -> Vec<String> {
    let mut argv = vec!["-t".to_string(), req.fstype.clone()];
    if !req.options.is_empty() {
        argv.push("-o".to_string());
        argv.push(req.options.clone());
    }
    argv.push("--".to_string());
    argv.push(req.source.clone());
    argv.push(req.target.clone());
    argv
}

/// Build the `umount(8)` argument vector. `-l` (lazy) + `-f` (force) detaches a
/// wedged/stale mount whose server is unreachable so the convergence loop can
/// re-mount; `--` guards the target. Mirrors the existing self-heal release.
pub fn umount_argv(target: &str) -> Vec<String> {
    vec![
        "-l".to_string(),
        "-f".to_string(),
        "--".to_string(),
        target.to_string(),
    ]
}

/// Realize `req` by exec-ing the native `mount` (root side). The mountpoint is
/// created first (a missing target is the common first-mount case); `mkdir -p`
/// is idempotent. Returns the trimmed stderr on failure so the convergence loop
/// can log why a source failed before advancing to the next.
pub async fn run_mount(req: &MountReq) -> Result<(), String> {
    if let Err(e) = tokio::fs::create_dir_all(&req.target).await {
        // A stale/wedged NFS handle at the mountpoint (dead server) makes the
        // path exist but not `stat`/traverse, so `create_dir_all` fails EEXIST —
        // the failure mode that wedged convergence: it could never re-create the
        // dir, so it could never remount. Force-release the target (`umount -lf`
        // is idempotent — "not mounted" is success) and retry once. A prior
        // plan()-emitted lazy Unmount may also still be detaching; this collapses
        // the release+remount so a stale handle self-heals in one tick.
        // Best-effort release; the create_dir_all retry below is authoritative
        // for whether we can proceed, so a umount error here is discarded.
        let _released = run_umount(&req.target).await;
        if let Err(e2) = tokio::fs::create_dir_all(&req.target).await {
            return Err(format!(
                "create mountpoint {}: {e2} (stale-release retry after: {e})",
                req.target
            ));
        }
    }
    // Materialize the backend-produced secret-file (0600, root) before mounting so
    // the mount helper can read it. Core validates the path against the secret-file
    // allowlist — it never trusts an arbitrary path from the plugin — but knows
    // nothing of the file's grammar (the plugin rendered `contents`).
    if let Some(sf) = &req.secret_file {
        if !plugin_toolkit::storage::is_valid_secret_file_path(&sf.path) {
            return Err(format!(
                "refused non-allowlisted secret-file path: {}",
                sf.path
            ));
        }
        if let Err(e) = write_secret_file(&sf.path, &sf.contents).await {
            return Err(format!("write secret-file {}: {e}", sf.path));
        }
    }
    exec("mount", &mount_argv(req)).await
}

/// Atomic 0600 write of a secret-file: sibling temp, chmod-before-rename so the
/// bytes are never visible at a laxer mode, then rename over the target. Mirrors
/// the autofs applier's creds-file write so the two paths behave identically.
async fn write_secret_file(path: &str, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = format!("{path}.orca.tmp");
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Release `target` by exec-ing the native `umount -lf`. Idempotent enough for
/// convergence: "not mounted" is treated as success so a redundant unmount of an
/// already-clean target does not surface as an error.
pub async fn run_umount(target: &str) -> Result<(), String> {
    match exec("umount", &umount_argv(target)).await {
        Ok(()) => Ok(()),
        Err(e) if e.contains("not mounted") || e.contains("not found") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Spawn `program` with `argv`, mapping a non-zero exit to its trimmed stderr.
async fn exec(program: &str, argv: &[String]) -> Result<(), String> {
    let out = Command::new(program)
        .args(argv)
        .output()
        .await
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(source: &str, opts: &str) -> MountReq {
        MountReq {
            source: source.to_string(),
            target: "/mnt/data".to_string(),
            fstype: "nfs4".to_string(),
            options: opts.to_string(),
            secret_file: None,
        }
    }

    #[test]
    fn mount_argv_full_spec() {
        let argv = mount_argv(&req(
            "10.10.10.10:/mnt/user/data",
            "vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30",
        ));
        assert_eq!(
            argv,
            [
                "-t",
                "nfs4",
                "-o",
                "vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30",
                "--",
                "10.10.10.10:/mnt/user/data",
                "/mnt/data",
            ]
        );
    }

    #[test]
    fn mount_argv_omits_dash_o_when_no_options() {
        let argv = mount_argv(&req("10.10.10.10:/mnt/user/data", ""));
        assert_eq!(
            argv,
            [
                "-t",
                "nfs4",
                "--",
                "10.10.10.10:/mnt/user/data",
                "/mnt/data"
            ]
        );
        assert!(!argv.iter().any(|a| a == "-o"), "no empty -o");
    }

    #[test]
    fn mount_argv_double_dash_precedes_source() {
        // `--` must sit immediately before source/target so a leading-dash path
        // can never be parsed as a flag.
        let argv = mount_argv(&req("10.10.10.10:/mnt/user/data", "ro"));
        let dd = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[dd + 1..], ["10.10.10.10:/mnt/user/data", "/mnt/data"]);
    }

    #[test]
    fn umount_argv_is_lazy_force_guarded() {
        assert_eq!(umount_argv("/mnt/data"), ["-l", "-f", "--", "/mnt/data"]);
    }
}
