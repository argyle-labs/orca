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
        return Err(format!("create mountpoint {}: {e}", req.target));
    }
    exec("mount", &mount_argv(req)).await
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
