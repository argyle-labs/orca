//! Plugin-facing helper for privileged in-container exec on Proxmox LXC.
//!
//! A plugin that manages an LXC deployment (Jellyfin, Kavita, …) needs to run a
//! command *inside* the container — `apt-get install --only-upgrade <pkg>`,
//! `systemctl restart <svc>`. That is `pct exec`, which needs root the non-root
//! orca daemon lacks. This helper routes the request through orca's scoped
//! privileged seam instead of shelling `pct` directly:
//!
//! * **With orca (normal):** the daemon set `ORCA_BIN` when it spawned the
//!   plugin, so the helper invokes `sudo -n <orca> admin lxc-exec` and pipes the
//!   op as JSON on stdin. The orca binary validates the command against a fixed
//!   allowlist and runs `pct exec` as root. The sudoers grant is scoped to that
//!   one subcommand — no raw `pct` rule.
//! * **Without orca (standalone):** run directly on a Proxmox host outside a
//!   daemon (`ORCA_BIN` unset), the helper falls back to `sudo -n pct exec` so
//!   the plugin's run-without-orca path keeps working.
//!
//! The wire shape (`{vmid, argv}` in, [`LxcExecResult`] out) mirrors
//! `system::lxc_exec` on the orca side; keep the two in sync.

use tokio::io::AsyncWriteExt;

/// Env var (set by the plugin supervisor) naming the orca binary that launched
/// this plugin. Mirrors `plugin_loader::supervisor::ORCA_BIN_ENV`.
pub const ORCA_BIN_ENV: &str = "ORCA_BIN";

/// Outcome of an in-container exec. Mirrors `system::lxc_exec::LxcExecResult`.
#[derive(Debug, Clone, Default, crate::serde::Serialize, crate::serde::Deserialize)]
#[serde(crate = "crate::serde")]
pub struct LxcExecResult {
    /// True when the command ran and exited zero.
    pub success: bool,
    /// Process exit code; `None` if it never ran or was killed by signal/timeout.
    pub exit_code: Option<i32>,
    /// Captured stdout (trimmed).
    pub stdout: String,
    /// Captured stderr (trimmed).
    pub stderr: String,
    /// Set when the op was refused or could not run; empty on a clean run.
    pub error: String,
}

/// Run `argv` inside LXC `vmid` through the privileged seam. `Ok` means the
/// round-trip completed and returns the container command's outcome (check
/// [`LxcExecResult::success`]); `Err` means the seam itself could not be
/// reached (spawn/parse failure). `argv[0]` must be on orca's lxc-exec allowlist
/// (apt-get/apt/dpkg/dpkg-query/systemctl/true) or the op is refused.
pub async fn lxc_exec(vmid: u32, argv: &[&str]) -> anyhow::Result<LxcExecResult> {
    match std::env::var(ORCA_BIN_ENV) {
        Ok(orca_bin) if !orca_bin.is_empty() => via_orca(&orca_bin, vmid, argv).await,
        _ => via_direct_pct(vmid, argv).await,
    }
}

/// Normal path: `sudo -n <orca> admin lxc-exec`, op piped on stdin, result JSON
/// parsed from stdout.
async fn via_orca(orca_bin: &str, vmid: u32, argv: &[&str]) -> anyhow::Result<LxcExecResult> {
    let payload = crate::serde_json::json!({ "vmid": vmid, "argv": argv }).to_string();

    let mut child = tokio::process::Command::new("sudo")
        .arg("-n")
        .arg(orca_bin)
        .args(["admin", "lxc-exec"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn `sudo {orca_bin} admin lxc-exec`: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).await.ok();
        stdin.shutdown().await.ok();
    }

    let out = child
        .wait_with_output()
        .await
        .map_err(|e| anyhow::anyhow!("wait lxc-exec helper: {e}"))?;
    if !out.status.success() {
        // Non-zero from the helper itself (e.g. sudo denied) — distinct from the
        // container command failing, which comes back as a parsed result.
        anyhow::bail!(
            "lxc-exec privileged helper failed (exit {}): {}. Is the `orca admin \
             lxc-exec` sudoers grant installed on this Proxmox host? (run `system \
             create --service-user <u>` to converge it)",
            out.status.code().map_or("signal".into(), |c| c.to_string()),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    crate::serde_json::from_slice::<LxcExecResult>(&out.stdout).map_err(|e| {
        anyhow::anyhow!(
            "parse lxc-exec result: {e}: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        )
    })
}

/// Fallback for run-without-orca: `sudo -n pct exec <vmid> -- <argv>` directly.
async fn via_direct_pct(vmid: u32, argv: &[&str]) -> anyhow::Result<LxcExecResult> {
    let mut cmd = tokio::process::Command::new("sudo");
    cmd.arg("-n")
        .arg("pct")
        .arg("exec")
        .arg(vmid.to_string())
        .arg("--")
        .args(argv)
        .kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawn `sudo pct exec`: {e}"))?;
    Ok(LxcExecResult {
        success: out.status.success(),
        exit_code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        error: String::new(),
    })
}
