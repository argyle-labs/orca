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
    /// Captured stdout, lossily decoded; exact bytes via [`LxcExecResult::stdout_bytes`].
    pub stdout: String,
    /// Captured stderr, lossily decoded; exact bytes via [`LxcExecResult::stderr_bytes`].
    pub stderr: String,
    /// Set when the op was refused or could not run; empty on a clean run.
    pub error: String,
    #[serde(default)]
    pub stdout_b64: String,
    #[serde(default)]
    pub stderr_b64: String,
    #[serde(default)]
    pub stdout_len: usize,
    #[serde(default)]
    pub stderr_len: usize,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
}

impl LxcExecResult {
    pub fn stdout_bytes(&self) -> anyhow::Result<Vec<u8>> {
        exact(&self.stdout_b64, &self.stdout)
    }

    pub fn stderr_bytes(&self) -> anyhow::Result<Vec<u8>> {
        exact(&self.stderr_b64, &self.stderr)
    }
}

fn exact(b64: &str, lossy: &str) -> anyhow::Result<Vec<u8>> {
    if b64.is_empty() {
        return Ok(lossy.as_bytes().to_vec());
    }
    utils::encoding::base64_decode(b64)
}

/// Run `argv` inside LXC `vmid` through the privileged seam. `Ok` means the
/// round-trip completed and returns the container command's outcome (check
/// [`LxcExecResult::success`]); `Err` means the seam itself could not be
/// reached (spawn/parse failure). `argv[0]` must be on orca's lxc-exec allowlist
/// (apt-get/apt/dpkg/dpkg-query/systemctl/true) or the op is refused.
pub async fn lxc_exec(vmid: u32, argv: &[&str]) -> anyhow::Result<LxcExecResult> {
    let payload = crate::serde_json::json!({ "vmid": vmid, "argv": argv }).to_string();
    if let Some(out) = crate::capsink::privileged_route(crate::abi::LXC_EXEC, &payload) {
        let out = out?;
        return helper_result(out.success, out.code, out.stdout.as_bytes(), &out.stderr);
    }
    match std::env::var(ORCA_BIN_ENV) {
        Ok(orca_bin) if !orca_bin.is_empty() => via_orca(&orca_bin, &payload).await,
        _ => via_direct_pct(vmid, argv).await,
    }
}

/// Normal path: `sudo -n <orca> admin lxc-exec`, op piped on stdin, result JSON
/// parsed from stdout.
async fn via_orca(orca_bin: &str, payload: &str) -> anyhow::Result<LxcExecResult> {
    let out = utils::privileged::run(utils::privileged::PrivilegedCall::new(
        [orca_bin, "admin", "lxc-exec"],
        payload,
        utils::privileged::LXC_EXEC_HELPER_TIMEOUT,
    ))
    .await
    .map_err(|e| anyhow::anyhow!("spawn `sudo {orca_bin} admin lxc-exec`: {e}"))?;
    helper_result(
        out.success,
        out.code,
        &out.stdout,
        &String::from_utf8_lossy(&out.stderr),
    )
}

fn helper_result(
    success: bool,
    code: Option<i32>,
    stdout: &[u8],
    stderr: &str,
) -> anyhow::Result<LxcExecResult> {
    if !success {
        // Non-zero from the helper itself (e.g. sudo denied) — distinct from the
        // container command failing, which comes back as a parsed result.
        anyhow::bail!(
            "lxc-exec privileged helper failed (exit {}): {}. Is the `orca admin \
             lxc-exec` sudoers grant installed on this Proxmox host? (run `system \
             create --service-user <u>` to converge it)",
            code.map_or("signal".into(), |c| c.to_string()),
            stderr.trim()
        );
    }
    crate::serde_json::from_slice::<LxcExecResult>(stdout).map_err(|e| {
        anyhow::anyhow!(
            "parse lxc-exec result: {e}: {}",
            String::from_utf8_lossy(stdout).trim()
        )
    })
}

/// Fallback for run-without-orca: `sudo -n pct exec <vmid> -- <argv>` directly.
async fn via_direct_pct(vmid: u32, argv: &[&str]) -> anyhow::Result<LxcExecResult> {
    let vmid = vmid.to_string();
    let out = utils::privileged::run(utils::privileged::PrivilegedCall::new(
        ["/usr/sbin/pct", "exec", vmid.as_str(), "--"]
            .into_iter()
            .chain(argv.iter().copied()),
        Vec::new(),
        utils::privileged::LXC_EXEC_HELPER_TIMEOUT,
    ))
    .await
    .map_err(|e| anyhow::anyhow!("spawn `sudo pct exec`: {e}"))?;
    Ok(LxcExecResult {
        success: out.success,
        exit_code: out.code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        error: String::new(),
        stdout_b64: utils::encoding::base64_encode(&out.stdout),
        stderr_b64: utils::encoding::base64_encode(&out.stderr),
        stdout_len: out.stdout.len(),
        stderr_len: out.stderr.len(),
        stdout_truncated: false,
        stderr_truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bytes_come_from_b64_and_fall_back_for_older_orca() {
        let r: LxcExecResult = crate::serde_json::from_str(
            r#"{"success":true,"exit_code":0,"stdout":"a\ufffd","stderr":"","error":"","stdout_b64":"Yf8K","stdout_len":3}"#,
        )
        .unwrap();
        assert_eq!(r.stdout_bytes().unwrap(), b"a\xff\n");
        let old: LxcExecResult = crate::serde_json::from_str(
            r#"{"success":true,"exit_code":0,"stdout":"hi","stderr":"","error":""}"#,
        )
        .unwrap();
        assert_eq!(old.stdout_bytes().unwrap(), b"hi");
        assert!(!old.stdout_truncated);
    }

    #[test]
    fn an_advertised_core_runs_lxc_exec_over_the_cap() {
        crate::capsink::set_core_privileged(&[crate::abi::LXC_EXEC.to_string()]);
        let sent = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let seen = sent.clone();
        let out = crate::capsink::with_cap_sink(
            Box::new(move |cap: &str, json: &str| {
                *seen.borrow_mut() = format!("{cap} {json}");
                Ok(r#"{"success":true,"code":0,"stdout":"{\"success\":true,\"exit_code\":0,\"stdout\":\"hi\",\"stderr\":\"\",\"error\":\"\"}","stderr":""}"#.into())
            }),
            || crate::reactor::block_on(lxc_exec(7, &["true"])),
        );
        crate::capsink::set_core_privileged(&[]);
        let out = out.unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, "hi");
        let sent = sent.borrow();
        let (cap, json) = sent.split_once(' ').unwrap();
        assert_eq!(cap, crate::abi::LXC_EXEC);
        let op: LxcOp = crate::serde_json::from_str(json).unwrap();
        assert_eq!((op.vmid, op.argv), (7, vec!["true".to_string()]));
    }

    #[derive(crate::serde::Deserialize)]
    #[serde(crate = "crate::serde")]
    struct LxcOp {
        vmid: u32,
        argv: Vec<String>,
    }

    #[test]
    fn a_failed_helper_is_an_error_not_a_result() {
        let err = helper_result(false, Some(1), b"", "sudo: a password is required").unwrap_err();
        assert!(err.to_string().contains("password is required"));
    }
}
