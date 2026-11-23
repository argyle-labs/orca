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
use std::time::Duration;
use tokio::process::Command;

/// Unbounded, a hung helper piles up in D-state and starves the host watchdog
/// into a softdog reboot (~60s). Stay under that margin.
const MOUNT_EXEC_TIMEOUT: Duration = Duration::from_secs(25);

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
    /// Placement id (UUID) the root ledger records for this mount; the only handle
    /// a later `Release` accepts.
    pub placement_id: String,
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
/// can never be read as a flag. `nosuid,nodev` is always appended so a
/// network mount can never carry setuid binaries or device nodes.
pub fn mount_argv(req: &MountReq) -> Vec<String> {
    let mut argv = vec!["-t".to_string(), req.fstype.clone()];
    argv.push("-o".to_string());
    argv.push(if req.options.is_empty() {
        "nosuid,nodev".to_string()
    } else {
        format!("{},nosuid,nodev", req.options)
    });
    argv.push("--".to_string());
    argv.push(req.source.clone());
    argv.push(req.target.clone());
    argv
}

/// Build the `mount --make-rshared <target>` argument vector. Applied after a
/// successful mount so the mountpoint carries `shared` propagation: when a
/// CIFS/NFS mount flaps and orca re-establishes it, the fresh superblock can
/// propagate into containers that bind subpaths as `rslave` — instead of leaving
/// them pinned to the detached old mount (persistent ENOENT until restart). See
/// #402. Propagation flags are per-mount and reset on remount, so this must be
/// re-applied on every (re)mount — which it is, since every mount flows here.
pub fn make_rshared_argv(target: &str) -> Vec<String> {
    vec!["--make-rshared".to_string(), target.to_string()]
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

/// `mkdir` on a wedged hard mount to a dead server blocks in D-state forever;
/// the blocking thread may linger but the async task gives up after this bound.
const MOUNTPOINT_PREP_TIMEOUT: Duration = Duration::from_secs(10);

/// Create the mountpoint, force-releasing a stale/wedged handle on error or
/// timeout (`umount -lf` is idempotent) and retrying once under the same bound.
/// A stale NFS/CIFS handle makes the path exist but fail `stat`, so without the
/// release convergence could never re-create the dir and never remount.
async fn prepare_mountpoint_with<M, MF, U, UF, D, DF>(
    target: &str,
    mkdir: M,
    release: U,
    deadline: D,
) -> Result<(), String>
where
    M: Fn(String) -> MF,
    MF: std::future::Future<Output = std::io::Result<()>>,
    U: Fn(String) -> UF,
    UF: std::future::Future<Output = Result<(), String>>,
    D: Fn() -> DF,
    DF: std::future::Future<Output = ()>,
{
    let bounded = || async {
        tokio::select! {
            r = mkdir(target.to_string()) => r.map_err(|e| e.to_string()),
            () = deadline() => Err("timed out; mountpoint unresponsive".to_string()),
        }
    };
    let Err(first) = bounded().await else {
        return Ok(());
    };
    // Best-effort: the retry below is authoritative for whether we can proceed.
    let _released = release(target.to_string()).await;
    bounded().await.map_err(|second| {
        format!("create mountpoint {target}: {second} (stale-release retry after: {first})")
    })
}

/// Realize `req` by exec-ing the native `mount` (root side). The mountpoint is
/// created first (a missing target is the common first-mount case); `mkdir -p`
/// is idempotent. Returns the trimmed stderr on failure so the convergence loop
/// can log why a source failed before advancing to the next.
pub async fn run_mount(req: &MountReq) -> Result<(), String> {
    prepare_mountpoint_with(
        &req.target,
        |t| async move {
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(t))
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        },
        |t| async move { run_umount(&t).await },
        || tokio::time::sleep(MOUNTPOINT_PREP_TIMEOUT),
    )
    .await?;
    // Materialize the backend-produced secret-file (0600, root) before mounting so
    // the mount helper can read it. Core validates the path against the secret-file
    // allowlist — it never trusts an arbitrary path from the plugin — but knows
    // nothing of the file's grammar (the plugin rendered `contents`).
    if let Some(sf) = &req.secret_file {
        materialize_secret_file(sf).await?;
    }
    exec("mount", &mount_argv(req)).await?;
    // why: make the mountpoint `shared` so a re-established mount after a flap
    // propagates into containers binding subpaths as `rslave`, instead of leaving
    // them pinned to the detached old superblock (persistent ENOENT until restart,
    // #402). Best-effort: the mount already succeeded and is the critical outcome,
    // so a propagation-setup failure must not fail the mount — only warn loudly.
    match exec("mount", &make_rshared_argv(&req.target)).await {
        Ok(()) => tracing::debug!(target = %req.target, "mountpoint set rshared"),
        Err(e) => tracing::warn!(
            target = %req.target,
            error = %e,
            "make-rshared failed; mount succeeded but stale-bind-after-remount protection (#402) not set"
        ),
    }
    Ok(())
}

pub(crate) async fn materialize_secret_file(sf: &SecretFile) -> Result<(), String> {
    if !plugin_toolkit::storage::is_valid_secret_file_path(&sf.path) {
        return Err(format!(
            "refused non-allowlisted secret-file path: {}",
            sf.path
        ));
    }
    crate::autofs::write_atomic(&sf.path, &sf.contents, Some(0o600))
        .await
        .map_err(|e| format!("write secret-file {}: {e}", sf.path))
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
    let child = Command::new(program)
        .args(argv)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;
    let out = match tokio::time::timeout(MOUNT_EXEC_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("wait {program}: {e}")),
        // Dropping `child` here triggers kill_on_drop.
        Err(_) => {
            return Err(format!(
                "{program} timed out after {}s (source unreachable) — killed",
                MOUNT_EXEC_TIMEOUT.as_secs()
            ));
        }
    };
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
            placement_id: "0192f5a0-0000-7000-8000-000000000001".to_string(),
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
            "10.0.0.10:/mnt/user/data",
            "vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30",
        ));
        assert_eq!(
            argv,
            [
                "-t",
                "nfs4",
                "-o",
                "vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,nosuid,nodev",
                "--",
                "10.0.0.10:/mnt/user/data",
                "/mnt/data",
            ]
        );
    }

    #[test]
    fn mount_argv_always_forces_nosuid_nodev() {
        let argv = mount_argv(&req("10.0.0.10:/mnt/user/data", ""));
        assert_eq!(
            argv,
            [
                "-t",
                "nfs4",
                "-o",
                "nosuid,nodev",
                "--",
                "10.0.0.10:/mnt/user/data",
                "/mnt/data"
            ]
        );
    }

    #[test]
    fn mount_argv_double_dash_precedes_source() {
        // `--` must sit immediately before source/target so a leading-dash path
        // can never be parsed as a flag.
        let argv = mount_argv(&req("10.0.0.10:/mnt/user/data", "ro"));
        let dd = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[dd + 1..], ["10.0.0.10:/mnt/user/data", "/mnt/data"]);
    }

    #[test]
    fn make_rshared_argv_names_target() {
        assert_eq!(
            make_rshared_argv("/mnt/data"),
            ["--make-rshared", "/mnt/data"]
        );
    }

    #[test]
    fn umount_argv_is_lazy_force_guarded() {
        assert_eq!(umount_argv("/mnt/data"), ["-l", "-f", "--", "/mnt/data"]);
    }

    fn dir_probe() -> (
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        Default::default()
    }

    #[tokio::test]
    async fn prepare_healthy_target_does_not_release() {
        use std::sync::atomic::Ordering::SeqCst;
        let (mk, rel) = dir_probe();
        let (m, r) = (mk.clone(), rel.clone());
        let out = prepare_mountpoint_with(
            "/mnt/x",
            move |_| {
                m.fetch_add(1, SeqCst);
                async { Ok(()) }
            },
            move |_| {
                r.fetch_add(1, SeqCst);
                async { Ok(()) }
            },
            std::future::pending::<()>,
        )
        .await;
        assert_eq!(out, Ok(()));
        assert_eq!((mk.load(SeqCst), rel.load(SeqCst)), (1, 0));
    }

    #[tokio::test]
    async fn prepare_mkdir_error_releases_then_retries() {
        use std::sync::atomic::Ordering::SeqCst;
        let (mk, rel) = dir_probe();
        let (m, r) = (mk.clone(), rel.clone());
        let out = prepare_mountpoint_with(
            "/mnt/x",
            move |_| {
                let n = m.fetch_add(1, SeqCst);
                async move {
                    if n == 0 {
                        Err(std::io::Error::other("stale"))
                    } else {
                        Ok(())
                    }
                }
            },
            move |_| {
                r.fetch_add(1, SeqCst);
                async { Ok(()) }
            },
            std::future::pending::<()>,
        )
        .await;
        assert_eq!(out, Ok(()));
        assert_eq!((mk.load(SeqCst), rel.load(SeqCst)), (2, 1));
    }

    #[tokio::test]
    async fn prepare_hung_mkdir_times_out_and_releases() {
        use std::sync::atomic::Ordering::SeqCst;
        let (_, rel) = dir_probe();
        let r = rel.clone();
        let out = prepare_mountpoint_with(
            "/mnt/x",
            |_| std::future::pending::<std::io::Result<()>>(),
            move |_| {
                r.fetch_add(1, SeqCst);
                async { Ok(()) }
            },
            || std::future::ready(()),
        )
        .await;
        let err = out.unwrap_err();
        assert!(err.contains("timed out") && err.contains("/mnt/x"), "{err}");
        assert_eq!(rel.load(SeqCst), 1);
    }
}
