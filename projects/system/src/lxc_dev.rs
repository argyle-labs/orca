//! Privileged LXC device passthrough — binds a host device node into a
//! Proxmox LXC guest's `devN` config slot.
//!
//! Proxmox hardcodes `devN` device-passthrough config edits to `root@pam`
//! (`PVE::LXC.pm` raises its permission exception unconditionally, with no
//! permission-system check at all), so orca's own automation token can never
//! satisfy this via a role grant. This mirrors the [`crate::lxc_exec`]
//! push/exec seam: a narrowly-scoped, allowlist-validated root-side helper
//! rather than a raw `pct` grant in sudoers.

use anyhow::{Result, bail};
use contract::ToolCtx;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A config set is fast; this is generous.
const DEV_PASSTHROUGH_TIMEOUT: Duration = Duration::from_secs(30);

/// Host device paths this seam permits binding into a guest. Extend
/// deliberately — every entry widens what a container can bind to raw host
/// hardware. Never add a block device, `/dev/mem`, `/dev/kmem`, or anything
/// under `/dev/disk/`, `/dev/sd*`, `/dev/nvme*` — those are host-breakout
/// vectors.
pub const DEV_PASSTHROUGH_ALLOWLIST: &[&str] = &["/dev/net/tun"];

/// A single privileged device passthrough: bind `dev_path` into LXC `vmid`'s
/// `dev0` slot. Supplied by the daemon; the root executor validates before
/// acting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LxcDevPassthroughOp {
    /// Target Proxmox LXC vmid.
    pub vmid: u32,
    /// Host device path to bind. Must be on [`DEV_PASSTHROUGH_ALLOWLIST`]
    /// exactly.
    pub dev_path: String,
}

/// Outcome of an [`LxcDevPassthroughOp`]. Mirrors [`crate::lxc_exec::LxcPushResult`]'s
/// shape: errors are collected into the payload, not thrown across the `sudo`
/// boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LxcDevPassthroughResult {
    /// True when the device was bound into the container config.
    pub ok: bool,
    /// Set when the op was refused or `pct set` failed; empty on success.
    pub error: String,
}

impl LxcDevPassthroughResult {
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

/// Validate an op against the allowlist without running it. Exact string
/// match against [`DEV_PASSTHROUGH_ALLOWLIST`] only — no prefix/glob
/// matching, so a near-miss like `/dev/net/tun2` is refused, not accepted.
pub fn validate(op: &LxcDevPassthroughOp) -> Result<(), String> {
    if op.dev_path.is_empty() {
        return Err("dev_path is empty".to_string());
    }
    if op.dev_path.split('/').any(|seg| seg == "..") {
        return Err(format!(
            "dev_path '{}' contains a '..' component",
            op.dev_path
        ));
    }
    if !DEV_PASSTHROUGH_ALLOWLIST.contains(&op.dev_path.as_str()) {
        return Err(format!(
            "dev_path '{}' is not on the device-passthrough allowlist ({})",
            op.dev_path,
            DEV_PASSTHROUGH_ALLOWLIST.join(", ")
        ));
    }
    Ok(())
}

/// Daemon-side bridge to the root helper: spawn `sudo -n <self> admin
/// lxc-dev-passthrough` and pipe the op as JSON on stdin, returning the parsed
/// [`LxcDevPassthroughResult`]. Sibling of [`crate::lxc_exec::run_privileged_lxc`];
/// a spawn/parse failure surfaces in the result's `error` rather than a panic.
pub async fn run_privileged_dev_passthrough(op: &LxcDevPassthroughOp) -> LxcDevPassthroughResult {
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => return LxcDevPassthroughResult::refused(format!("serialize op: {e}")),
    };
    let call = utils::privileged::PrivilegedCall::helper(
        "lxc-dev-passthrough",
        payload,
        utils::privileged::LXC_DEV_PASSTHROUGH_HELPER_TIMEOUT,
    );
    match utils::privileged::run(call).await {
        Ok(out) if out.success => serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            LxcDevPassthroughResult::refused(format!(
                "parse helper output: {e}: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ))
        }),
        Ok(out) => LxcDevPassthroughResult::refused(format!(
            "helper exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            LxcDevPassthroughResult::refused(format!(
                "lxc-dev-passthrough helper did not finish within {}s",
                utils::privileged::LXC_DEV_PASSTHROUGH_HELPER_TIMEOUT.as_secs()
            ))
        }
        Err(e) => LxcDevPassthroughResult::refused(format!("run sudo helper: {e}")),
    }
}

/// Execute a validated [`LxcDevPassthroughOp`] as root via `pct set`. Called
/// only from the `orca admin lxc-dev` CLI path (behind `sudo -n`). Validates
/// the allowlist, then runs `pct set <vmid> -dev0 <dev_path>`. Never panics —
/// every failure is returned in the result so the daemon sees a structured
/// outcome across the `sudo` boundary.
///
/// Always targets `dev0`; does not check for an existing `dev0` on the guest.
/// A second call would overwrite slot 0 rather than add a second passthrough
/// device — fine for today's single-device use case, revisit if ever asked to
/// support more than one passthrough device per guest.
pub async fn execute_privileged_dev_passthrough(
    op: LxcDevPassthroughOp,
) -> LxcDevPassthroughResult {
    if let Err(e) = validate(&op) {
        return LxcDevPassthroughResult::refused(e);
    }
    let mut cmd = match utils::privileged::host_command("pct") {
        Ok(c) => c,
        Err(e) => return LxcDevPassthroughResult::refused(e.to_string()),
    };

    cmd.args(["set", &op.vmid.to_string(), "-dev0", &op.dev_path]);

    let child = cmd
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return LxcDevPassthroughResult::refused(format!("spawn pct: {e}")),
    };

    match tokio::time::timeout(DEV_PASSTHROUGH_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => LxcDevPassthroughResult::ok(),
        Ok(Ok(out)) => LxcDevPassthroughResult::refused(format!(
            "pct set exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Err(e)) => LxcDevPassthroughResult::refused(format!("wait pct: {e}")),
        Err(_) => LxcDevPassthroughResult::refused(format!(
            "pct set timed out after {}s — killed",
            DEV_PASSTHROUGH_TIMEOUT.as_secs()
        )),
    }
}

// ── Tool surface ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LxcDevPassthroughArgs {
    /// Target Proxmox LXC vmid.
    #[arg(long)]
    pub vmid: u32,
    /// Host device path to bind. Must be on [`DEV_PASSTHROUGH_ALLOWLIST`] exactly.
    #[arg(long)]
    pub dev_path: String,
}

/// [MUTATES STATE] Bind a host device node into an LXC guest's `dev0` slot —
/// mirrors the `orca admin lxc-dev-passthrough` privilege boundary. Admin-only
/// and side-effecting; the device path is checked against a fixed allowlist
/// root-side.
#[orca_tool(domain = "lxc", verb = "dev_passthrough", role = "admin")]
async fn lxc_dev_passthrough(args: LxcDevPassthroughArgs, _ctx: &ToolCtx) -> Result<()> {
    let op = LxcDevPassthroughOp {
        vmid: args.vmid,
        dev_path: args.dev_path,
    };
    let res = run_privileged_dev_passthrough(&op).await;
    if !res.ok {
        bail!("{}", res.error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(dev_path: &str) -> LxcDevPassthroughOp {
        LxcDevPassthroughOp {
            vmid: 113,
            dev_path: dev_path.to_string(),
        }
    }

    #[test]
    fn allowlist_accepts_exact_match() {
        assert!(validate(&op("/dev/net/tun")).is_ok());
    }

    #[test]
    fn allowlist_rejects_empty() {
        let err = validate(&op("")).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn allowlist_rejects_sensitive_devices() {
        for path in ["/dev/mem", "/dev/sda"] {
            let err = validate(&op(path)).unwrap_err();
            assert!(
                err.contains("not on the device-passthrough allowlist"),
                "{err}"
            );
        }
    }

    #[test]
    fn allowlist_rejects_near_miss_exact_match_only() {
        for path in ["/dev/net/tun2", "/dev/net/tunfoo"] {
            let err = validate(&op(path)).unwrap_err();
            assert!(
                err.contains("not on the device-passthrough allowlist"),
                "{err}"
            );
        }
    }

    #[test]
    fn allowlist_rejects_traversal() {
        let err = validate(&op("/dev/net/tun/../../etc/shadow")).unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn allowlist_rejects_non_dev_path() {
        let err = validate(&op("/etc/shadow")).unwrap_err();
        assert!(
            err.contains("not on the device-passthrough allowlist"),
            "{err}"
        );
    }

    #[test]
    fn allowlist_rejects_trailing_space() {
        // Exact-match only: no trimming, so a trailing space is a different
        // string and is refused rather than silently normalized.
        let err = validate(&op("/dev/net/tun ")).unwrap_err();
        assert!(
            err.contains("not on the device-passthrough allowlist"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn refused_dev_path_never_spawns_pct() {
        let res = execute_privileged_dev_passthrough(op("/dev/sda")).await;
        assert!(!res.ok);
        assert!(
            res.error
                .contains("not on the device-passthrough allowlist"),
            "{}",
            res.error
        );
    }

    #[tokio::test]
    async fn a_valid_op_never_spawns_pct_under_test() {
        let res = execute_privileged_dev_passthrough(op("/dev/net/tun")).await;
        assert!(!res.ok);
        assert!(
            res.error.contains("refusing to run host-mutating `pct`"),
            "{}",
            res.error
        );
    }
}
