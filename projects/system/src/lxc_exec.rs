//! Privileged in-container exec for Proxmox LXC guests — the `pct exec` analogue
//! of the autofs `storage-apply` seam.
//!
//! Managing a Jellyfin (or any) deployment that lives in an LXC means running a
//! command *inside* the container: `apt-get install --only-upgrade jellyfin`,
//! `systemctl restart jellyfin`, etc. On Proxmox that is `pct exec <vmid> -- …`,
//! which needs root/pmxcfs access the non-root orca daemon does not have. Rather
//! than grant the daemon raw `pct` in sudoers (a broad, wildcard privilege), the
//! daemon shells `sudo -n <orca> admin lxc-exec` and the payload rides on stdin —
//! exactly the [`crate::autofs`] `run_privileged` / `execute_privileged` model,
//! so the sudoers grant stays scoped to one orca subcommand with no wildcard.
//!
//! **Defense in depth (root side).** The privileged executor never trusts the
//! op blindly: the target `vmid` must be numeric (it is a `u32`, so this is by
//! construction) and the command basename (`argv[0]`) must be on
//! [`ALLOWED_COMMANDS`] — the small set the deployment-lifecycle tools actually
//! need. A compromised or buggy plugin cannot turn this seam into arbitrary
//! root-in-container exec.
//!
//! Runs **root-side only**, inside the `orca admin lxc-exec` helper behind the
//! `sudo -n` boundary. The daemon-side bridge is [`crate::autofs::run_privileged`]'s
//! sibling; plugins reach it through `plugin_toolkit`'s `lxc_exec` helper (which
//! spawns the same `sudo -n <orca> admin lxc-exec`), never this module directly.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;

/// In-container work can be slow (`apt-get update && install`), but must stay
/// bounded so a hung `pct exec` never piles up in D-state and starves the host
/// watchdog. Five minutes covers a package upgrade with margin.
const LXC_EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// Command basenames the seam permits as `argv[0]` inside the container. Scoped
/// to the deployment-lifecycle verbs (package upgrade + service restart + a
/// couple of read-only probes). Extend deliberately — every entry widens what a
/// plugin can run as root inside a container.
pub const ALLOWED_COMMANDS: &[&str] = &[
    "apt-get",
    "apt",
    "dpkg",
    "dpkg-query",
    "systemctl",
    "true", // no-op used by capability preflights
    // Read-only diagnostics for in-guest monitoring (disk usage, file/log peeks).
    "df",
    "cat",
    "ls",
    "stat",
    "head",
    "tail",
];

/// A single privileged in-container exec: run `argv` inside LXC `vmid`. Both
/// fields are supplied by the daemon; the root executor validates them before
/// acting. `argv[0]` is the program, the rest its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LxcExecOp {
    /// Target Proxmox LXC vmid.
    pub vmid: u32,
    /// Command + args to run inside the container. `argv[0]` must be on
    /// [`ALLOWED_COMMANDS`].
    pub argv: Vec<String>,
}

/// Outcome of an [`LxcExecOp`]. Mirrors the autofs result shape (errors are
/// collected into the payload, not thrown across the `sudo` boundary): a
/// spawn/validation failure lands in `error` with `exit_code: None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LxcExecResult {
    /// True when the command ran and exited zero.
    pub success: bool,
    /// Process exit code; `None` if it never ran (validation/spawn failure) or
    /// was killed by signal/timeout.
    pub exit_code: Option<i32>,
    /// Captured stdout (trimmed).
    pub stdout: String,
    /// Captured stderr (trimmed).
    pub stderr: String,
    /// Set when the op was refused or could not run; empty on a clean run.
    pub error: String,
}

impl LxcExecResult {
    fn refused(msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            ..Default::default()
        }
    }
}

/// The basename of a command path, for the allowlist check. `apt-get` and
/// `/usr/bin/apt-get` both resolve to `apt-get`, so an absolute path cannot
/// smuggle a non-allowlisted binary past the check.
fn basename(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

/// Validate an op against the allowlist without running it. Exposed so the
/// daemon-side bridge (and tests) can reject early with the same rule the root
/// executor enforces.
pub fn validate(op: &LxcExecOp) -> Result<(), String> {
    let prog = op
        .argv
        .first()
        .ok_or_else(|| "empty argv: no command to run".to_string())?;
    let base = basename(prog);
    if !ALLOWED_COMMANDS.contains(&base) {
        return Err(format!(
            "refused command '{base}': not on the lxc-exec allowlist ({})",
            ALLOWED_COMMANDS.join(", ")
        ));
    }
    Ok(())
}

/// Daemon-side bridge to the root helper: spawn `sudo -n <self> admin lxc-exec`
/// and pipe the op as JSON on stdin, returning the parsed [`LxcExecResult`]. The
/// non-root daemon can't run `pct` itself, so it rides the same scoped sudoers
/// grant `orca admin lxc-exec` exposes — the sibling of [`crate::autofs::run_privileged`].
/// A spawn/parse failure surfaces in the result's `error` rather than a panic.
pub async fn run_privileged_lxc(op: &LxcExecOp) -> LxcExecResult {
    use tokio::io::AsyncWriteExt;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return LxcExecResult::refused(format!("resolve current exe: {e}")),
    };
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => return LxcExecResult::refused(format!("serialize op: {e}")),
    };

    let mut child = match Command::new("sudo")
        .arg("-n")
        .arg(&exe)
        .args(["admin", "lxc-exec"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return LxcExecResult::refused(format!("spawn sudo helper: {e}")),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _written = stdin.write_all(&payload).await;
        let _shut = stdin.shutdown().await;
    }

    match child.wait_with_output().await {
        Ok(out) if out.status.success() => {
            serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
                LxcExecResult::refused(format!(
                    "parse helper output: {e}: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                ))
            })
        }
        Ok(out) => LxcExecResult::refused(format!(
            "helper exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => LxcExecResult::refused(format!("run sudo helper: {e}")),
    }
}

/// Execute a validated [`LxcExecOp`] as root via `pct exec`. Called only from the
/// `orca admin lxc-exec` CLI path (behind `sudo -n`). Validates the command
/// allowlist, then runs `pct exec <vmid> -- <argv>` with a bounded timeout,
/// capturing stdout/stderr. Never panics — every failure is returned in the
/// result so the daemon sees a structured outcome across the `sudo` boundary.
pub async fn execute_privileged_lxc(op: LxcExecOp) -> LxcExecResult {
    if let Err(e) = validate(&op) {
        return LxcExecResult::refused(e);
    }

    // `pct exec <vmid> -- <argv...>` — the `--` terminates pct's own option
    // parsing so an argv starting with `-` is passed through to the container.
    let mut pct_argv: Vec<String> = vec!["exec".to_string(), op.vmid.to_string(), "--".to_string()];
    pct_argv.extend(op.argv.iter().cloned());

    let child = Command::new("pct")
        .args(&pct_argv)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return LxcExecResult::refused(format!("spawn pct: {e}")),
    };

    let out = match tokio::time::timeout(LXC_EXEC_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return LxcExecResult::refused(format!("wait pct: {e}")),
        Err(_) => {
            return LxcExecResult::refused(format!(
                "pct exec timed out after {}s — killed",
                LXC_EXEC_TIMEOUT.as_secs()
            ));
        }
    };

    LxcExecResult {
        success: out.status.success(),
        exit_code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        error: String::new(),
    }
}

// ── File push (write_file) ────────────────────────────────────────────────────
//
// `write_file` deliberately does NOT ride the exec allowlist: routing file bytes
// through `pct exec` would force allowlisting a shell/`tee`/`dd` and blow the
// scoped seam wide open. Instead this is a separate, confined `pct push` op —
// the daemon hands over `{vmid, path, contents, mode?, owner?}` and the root side
// materializes a host temp file *it alone names* (O_EXCL, 0600, in a root-owned
// state dir), pushes it into the guest, and always unlinks it.

/// Hard cap on pushed file size. A config/secret file is small; a multi-MiB cap
/// keeps a buggy or hostile caller from spilling the host temp dir. Reject over.
const LXC_PUSH_MAX_BYTES: usize = 8 * 1024 * 1024;

/// `pct push` is a local file copy into the container rootfs — fast, but bounded
/// so a wedged push never piles up in D-state.
const LXC_PUSH_TIMEOUT: Duration = Duration::from_secs(60);

/// A single privileged file push: write `contents` to `path` inside LXC `vmid`,
/// applying `mode`/`owner` natively via `pct push`. Supplied by the daemon; the
/// root executor validates before acting. `contents` is redacted from `Debug` so
/// a secret payload never lands in a log line or `{:?}` error.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LxcPushOp {
    /// Target Proxmox LXC vmid.
    pub vmid: u32,
    /// Absolute destination path inside the container.
    pub path: String,
    /// File contents. May carry a secret — redacted from `Debug`.
    pub contents: Vec<u8>,
    /// POSIX mode as an octal string (e.g. `"0640"`). `None` → guest default.
    pub mode: Option<String>,
    /// Owner as `user` or `user:group`. `None` → guest default (root).
    pub owner: Option<String>,
}

impl std::fmt::Debug for LxcPushOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LxcPushOp")
            .field("vmid", &self.vmid)
            .field("path", &self.path)
            .field("contents", &format!("<{} bytes>", self.contents.len()))
            .field("mode", &self.mode)
            .field("owner", &self.owner)
            .finish()
    }
}

/// Outcome of an [`LxcPushOp`]. Errors are collected into the payload (not thrown
/// across the `sudo` boundary), mirroring [`LxcExecResult`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LxcPushResult {
    /// True when the file was written into the container.
    pub ok: bool,
    /// Set when the op was refused or `pct push` failed; empty on success.
    pub error: String,
}

impl LxcPushResult {
    fn ok() -> Self {
        Self {
            ok: true,
            error: String::new(),
        }
    }
    fn refused(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: msg.into(),
        }
    }
}

/// Split an `owner` string into `(--user, --group?)` arguments. `"u:g"` →
/// `("u", Some("g"))`, `"u"` → `("u", None)`; empty/`None`/blank → `None` (apply
/// no ownership). Pure so it is unit-testable without a push.
fn split_owner(owner: Option<&str>) -> Option<(String, Option<String>)> {
    let raw = owner?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.split_once(':') {
        Some((u, g)) if !g.trim().is_empty() => {
            Some((u.trim().to_string(), Some(g.trim().to_string())))
        }
        Some((u, _)) => Some((u.trim().to_string(), None)),
        None => Some((raw.to_string(), None)),
    }
}

/// Validate a guest destination path: must be absolute and carry no `..`
/// component (a traversal segment could escape the intended target). The path is
/// the *guest-side* destination `pct push` writes to; keep it well-formed. Pure.
fn validate_push_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("guest path '{path}' is not absolute"));
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err(format!("guest path '{path}' contains a '..' component"));
    }
    Ok(())
}

/// Root-owned directory for host temp files staged before `pct push`. Under
/// orca's own state dir (never /tmp — /tmp is RAM-backed/world-writable on some
/// hosts), created 0700 so only root can see staged content.
fn push_staging_dir() -> Result<std::path::PathBuf, String> {
    let dir = contract::config::paths::state_dir()
        .map_err(|e| format!("resolve orca state dir: {e}"))?
        .join("lxc-push");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create staging dir: {e}"))?;
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|e| format!("chmod staging dir: {e}"))?;
    Ok(dir)
}

/// Removes its path on drop so an early return still unlinks the host temp file.
struct TempFileGuard(std::path::PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            tracing::warn!(path = %self.0.display(), error = %e, "unlink lxc-push host temp");
        }
    }
}

/// Daemon-side bridge: spawn `sudo -n <self> admin lxc-push` and pipe the op as
/// JSON on stdin, returning the parsed [`LxcPushResult`]. Exact sibling of
/// [`run_privileged_lxc`]; a spawn/parse failure surfaces in `error`, never a
/// panic. The op's `contents` ride stdin, never argv or a log line.
pub async fn run_privileged_lxc_push(op: &LxcPushOp) -> LxcPushResult {
    use tokio::io::AsyncWriteExt;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return LxcPushResult::refused(format!("resolve current exe: {e}")),
    };
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => return LxcPushResult::refused(format!("serialize op: {e}")),
    };

    let mut child = match Command::new("sudo")
        .arg("-n")
        .arg(&exe)
        .args(["admin", "lxc-push"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return LxcPushResult::refused(format!("spawn sudo helper: {e}")),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _written = stdin.write_all(&payload).await;
        let _shut = stdin.shutdown().await;
    }

    match child.wait_with_output().await {
        Ok(out) if out.status.success() => {
            serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
                LxcPushResult::refused(format!(
                    "parse helper output: {e}: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                ))
            })
        }
        Ok(out) => LxcPushResult::refused(format!(
            "helper exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => LxcPushResult::refused(format!("run sudo helper: {e}")),
    }
}

/// Execute an [`LxcPushOp`] as root via `pct push`. Called only from the
/// `orca admin lxc-push` CLI path (behind `sudo -n`). SAFETY-CRITICAL: the host
/// staging path is chosen entirely root-side (O_EXCL + 0600 in a root-owned dir),
/// never caller-supplied — a predictable/influenced host path would be a root
/// arbitrary-write/TOCTOU vector. The temp file is always unlinked. Never panics.
pub async fn execute_privileged_lxc_push(op: LxcPushOp) -> LxcPushResult {
    if let Err(e) = validate_push_path(&op.path) {
        return LxcPushResult::refused(e);
    }
    if op.contents.len() > LXC_PUSH_MAX_BYTES {
        return LxcPushResult::refused(format!(
            "contents {} bytes exceeds the {LXC_PUSH_MAX_BYTES}-byte push cap",
            op.contents.len()
        ));
    }

    let dir = match push_staging_dir() {
        Ok(d) => d,
        Err(e) => return LxcPushResult::refused(e),
    };

    // Root-chosen host temp path: unpredictable name, created O_EXCL so we never
    // follow or clobber a pre-existing path, mode 0600 so only root can read it.
    let name = format!(
        "push-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let host_tmp = dir.join(name);

    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&host_tmp)
        {
            Ok(f) => f,
            Err(e) => return LxcPushResult::refused(format!("create host temp file: {e}")),
        };
        // Guard is armed the moment the file exists, so every path below unlinks it.
        let _guard = TempFileGuard(host_tmp.clone());
        if let Err(e) = f.write_all(&op.contents).and_then(|()| f.flush()) {
            return LxcPushResult::refused(format!("write host temp file: {e}"));
        }
        drop(f);

        let mut argv: Vec<String> = vec![
            "push".to_string(),
            op.vmid.to_string(),
            host_tmp.to_string_lossy().into_owned(),
            op.path.clone(),
        ];
        if let Some(mode) = op.mode.as_deref().filter(|m| !m.trim().is_empty()) {
            argv.push("--perms".to_string());
            argv.push(mode.trim().to_string());
        }
        if let Some((user, group)) = split_owner(op.owner.as_deref()) {
            argv.push("--user".to_string());
            argv.push(user);
            if let Some(group) = group {
                argv.push("--group".to_string());
                argv.push(group);
            }
        }

        let child = Command::new("pct")
            .args(&argv)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => return LxcPushResult::refused(format!("spawn pct: {e}")),
        };

        match tokio::time::timeout(LXC_PUSH_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(out)) if out.status.success() => LxcPushResult::ok(),
            Ok(Ok(out)) => LxcPushResult::refused(format!(
                "pct push exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Ok(Err(e)) => LxcPushResult::refused(format!("wait pct: {e}")),
            Err(_) => LxcPushResult::refused(format!(
                "pct push timed out after {}s — killed",
                LXC_PUSH_TIMEOUT.as_secs()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_bare_and_absolute() {
        assert!(
            validate(&LxcExecOp {
                vmid: 113,
                argv: vec!["apt-get".into(), "update".into()],
            })
            .is_ok()
        );
        assert!(
            validate(&LxcExecOp {
                vmid: 113,
                argv: vec![
                    "/usr/bin/systemctl".into(),
                    "restart".into(),
                    "jellyfin".into()
                ],
            })
            .is_ok()
        );
    }

    #[test]
    fn allowlist_accepts_readonly_diagnostics() {
        // Read-only probes added to unblock in-guest disk/log monitoring.
        for cmd in ["df", "cat", "ls", "stat", "head", "tail", "systemctl"] {
            assert!(
                validate(&LxcExecOp {
                    vmid: 100,
                    argv: vec![cmd.into(), "example".into()],
                })
                .is_ok(),
                "{cmd} should be allowlisted"
            );
        }
        // A clearly-destructive command stays rejected.
        assert!(
            validate(&LxcExecOp {
                vmid: 100,
                argv: vec!["rm".into(), "-rf".into(), "/".into()],
            })
            .is_err()
        );
    }

    #[test]
    fn allowlist_rejects_arbitrary_and_empty() {
        let err = validate(&LxcExecOp {
            vmid: 113,
            argv: vec!["/bin/sh".into(), "-c".into(), "rm -rf /".into()],
        })
        .unwrap_err();
        assert!(err.contains("not on the lxc-exec allowlist"), "{err}");

        let err = validate(&LxcExecOp {
            vmid: 113,
            argv: vec![],
        })
        .unwrap_err();
        assert!(err.contains("empty argv"), "{err}");
    }

    #[test]
    fn split_owner_variants() {
        assert_eq!(
            split_owner(Some("root:root")),
            Some(("root".to_string(), Some("root".to_string())))
        );
        assert_eq!(split_owner(Some("root")), Some(("root".to_string(), None)));
        // A trailing colon with no group applies just the user.
        assert_eq!(split_owner(Some("root:")), Some(("root".to_string(), None)));
        assert_eq!(split_owner(Some("")), None);
        assert_eq!(split_owner(Some("   ")), None);
        assert_eq!(split_owner(None), None);
    }

    #[test]
    fn validate_push_path_rules() {
        assert!(validate_push_path("/etc/example.conf").is_ok());
        assert!(validate_push_path("etc/example.conf").is_err());
        assert!(validate_push_path("../etc/example.conf").is_err());
        assert!(validate_push_path("/etc/../../root/.ssh/authorized_keys").is_err());
    }

    #[test]
    fn push_op_debug_redacts_contents() {
        let op = LxcPushOp {
            vmid: 100,
            path: "/etc/example.conf".into(),
            contents: b"password=hunter2".to_vec(),
            mode: Some("0640".into()),
            owner: Some("root:root".into()),
        };
        let dbg = format!("{op:?}");
        assert!(!dbg.contains("hunter2"), "contents must be redacted");
        assert!(dbg.contains("<16 bytes>"));
    }

    #[tokio::test]
    async fn push_rejects_relative_path_before_pct() {
        let res = execute_privileged_lxc_push(LxcPushOp {
            vmid: 100,
            path: "etc/example.conf".into(),
            contents: b"x".to_vec(),
            mode: None,
            owner: None,
        })
        .await;
        assert!(!res.ok);
        assert!(res.error.contains("not absolute"), "{}", res.error);
    }

    #[tokio::test]
    async fn refused_command_never_spawns_pct() {
        // A non-allowlisted command must be refused before any pct spawn, so this
        // returns a clean refusal even on a host with no pct.
        let res = execute_privileged_lxc(LxcExecOp {
            vmid: 113,
            argv: vec!["curl".into(), "evil.example".into()],
        })
        .await;
        assert!(!res.success);
        assert!(
            res.error.contains("not on the lxc-exec allowlist"),
            "{}",
            res.error
        );
        assert_eq!(res.exit_code, None);
    }
}
