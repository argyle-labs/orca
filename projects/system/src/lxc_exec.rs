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
