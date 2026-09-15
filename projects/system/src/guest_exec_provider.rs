//! Core-owned `guest_exec` provider backed by orca's privileged LXC seam.
//!
//! This is the in-tree counterpart to the plugin-loaded [`contract::guest_exec`]
//! providers: rather than a subprocess plugin, core registers a `"proxmox"`
//! provider at daemon startup that routes an [`ExecRequest`] straight to the
//! existing `orca admin lxc-exec` seam (via [`crate::lxc_exec::run_privileged_lxc`]).
//! The exec surface stays allowlisted by that seam — a request whose `argv[0]`
//! is not on [`crate::lxc_exec::ALLOWED_COMMANDS`] is refused root-side.
//!
//! `write_file` routes through a dedicated `pct push` seam (`orca admin
//! lxc-push`) — deliberately NOT the exec allowlist, so file bytes never force
//! allowlisting a shell/`tee`/`dd`.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use contract::guest_exec::{
    ExecOutput, ExecRequest, GuestExec, GuestRef, WriteFileRequest, register_provider,
};
use contract::{BoxFuture, ToolCtx};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::lxc_exec::{
    LxcExecOp, LxcExecResult, LxcPushOp, run_privileged_lxc, run_privileged_lxc_push,
};

/// Registry name of this provider.
pub const PROVIDER_NAME: &str = "proxmox";

/// A [`GuestExec`] whose `exec` runs inside a Proxmox LXC through the scoped
/// `orca admin lxc-exec` sudoers seam. LXC-only in this slice (the VM/guest-agent
/// path is a later provider); the trait shape is transport-agnostic.
struct ProxmoxGuestExec;

/// Build the privileged LXC op from a transport-agnostic request. The guest's
/// `id` is the LXC vmid (numeric); `req.command` is the argv the seam validates
/// against its allowlist. Pure so the mapping is unit-testable without a seam.
fn op_from(guest: &GuestRef, req: &ExecRequest) -> Result<LxcExecOp> {
    let vmid: u32 = guest
        .id
        .parse()
        .with_context(|| format!("guest id '{}' is not a numeric LXC vmid", guest.id))?;
    Ok(LxcExecOp {
        vmid,
        argv: req.command.clone(),
    })
}

/// Build the privileged LXC push op from a transport-agnostic write request. The
/// guest's `id` is the LXC vmid (numeric); `path`/`mode`/`owner` pass through to
/// the root `pct push` executor. Pure so the mapping is unit-testable.
fn push_op_from(guest: &GuestRef, req: WriteFileRequest) -> Result<LxcPushOp> {
    let vmid: u32 = guest
        .id
        .parse()
        .with_context(|| format!("guest id '{}' is not a numeric LXC vmid", guest.id))?;
    Ok(LxcPushOp {
        vmid,
        path: req.path,
        contents: req.contents,
        mode: req.mode,
        owner: req.owner,
    })
}

/// Map the seam's result into the trait's [`ExecOutput`]. `timed_out` reflects the
/// caller-side deadline elapsing (the seam itself has its own hard cap).
fn output_from(res: LxcExecResult) -> ExecOutput {
    ExecOutput {
        exit_code: res.exit_code.map(i64::from),
        signal: None,
        stdout: res.stdout,
        stderr: res.stderr,
        timed_out: false,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

impl GuestExec for ProxmoxGuestExec {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn exec(&self, guest: GuestRef, req: ExecRequest) -> BoxFuture<'_, Result<ExecOutput>> {
        Box::pin(async move {
            let op = op_from(&guest, &req)?;
            let timeout = std::time::Duration::from_millis(req.timeout_ms());
            match tokio::time::timeout(timeout, run_privileged_lxc(&op)).await {
                Ok(res) => {
                    // A seam-level failure (validation/spawn) carries no exit code;
                    // surface it as an error rather than an empty success.
                    if !res.error.is_empty() && res.exit_code.is_none() {
                        bail!("{}", res.error);
                    }
                    Ok(output_from(res))
                }
                Err(_) => Ok(ExecOutput {
                    timed_out: true,
                    ..Default::default()
                }),
            }
        })
    }

    fn write_file(&self, guest: GuestRef, req: WriteFileRequest) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let op = push_op_from(&guest, req)?;
            let res = run_privileged_lxc_push(&op).await;
            if !res.ok {
                bail!("{}", res.error);
            }
            Ok(())
        })
    }
}

// ── Tool surface ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GuestExecArgs {
    /// Target guest id (an LXC vmid for the `proxmox` provider).
    #[arg(long)]
    pub id: String,
    /// Command + args to run inside the guest. `argv[0]` must be on the
    /// lxc-exec allowlist or the seam refuses it.
    #[arg(long = "arg", value_name = "ARG")]
    pub argv: Vec<String>,
    /// Overall deadline in milliseconds. Omit for the provider default.
    #[arg(long)]
    pub timeout_ms: Option<u64>,
}

/// Run an allowlisted command inside a guest via the core-owned `proxmox`
/// provider, returning its captured stdout/stderr/exit code. Admin-only and
/// side-effecting — mirrors the `orca admin lxc-exec` privilege boundary.
#[orca_tool(domain = "guest", verb = "exec", data_mutation = true, role = "admin")]
async fn guest_exec_run(args: GuestExecArgs, _ctx: &ToolCtx) -> Result<ExecOutput> {
    let guest = GuestRef {
        id: args.id,
        ..Default::default()
    };
    let req = ExecRequest {
        command: args.argv,
        timeout_ms: args.timeout_ms,
        ..Default::default()
    };
    contract::guest_exec::exec(PROVIDER_NAME, guest, req).await
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GuestWriteFileArgs {
    /// Target guest id (an LXC vmid for the `proxmox` provider).
    #[arg(long)]
    pub id: String,
    /// Absolute destination path inside the guest.
    #[arg(long)]
    pub path: String,
    /// File contents. Slice-2 CLI surface takes UTF-8 text mapped to bytes; a
    /// future slice can add a base64/`@file` form for binary/secret payloads.
    #[arg(long)]
    pub contents: String,
    /// POSIX mode as an octal string (e.g. `0640`). Omit for the guest default.
    #[arg(long)]
    pub mode: Option<String>,
    /// Owner as `user` or `user:group`. Omit for the guest default.
    #[arg(long)]
    pub owner: Option<String>,
}

/// Write a file into a guest via the core-owned `proxmox` provider's confined
/// `pct push` seam. Admin-only and side-effecting — mirrors the `orca admin
/// lxc-push` privilege boundary; the payload never rides argv.
#[orca_tool(
    domain = "guest",
    verb = "write_file",
    data_mutation = true,
    role = "admin"
)]
async fn guest_write_file(args: GuestWriteFileArgs, _ctx: &ToolCtx) -> Result<()> {
    let guest = GuestRef {
        id: args.id,
        ..Default::default()
    };
    let req = WriteFileRequest {
        path: args.path,
        contents: args.contents.into_bytes(),
        mode: args.mode,
        owner: args.owner,
    };
    contract::guest_exec::write_file(PROVIDER_NAME, guest, req).await
}

/// Register the core-owned `proxmox` guest-exec provider. Called once at daemon
/// startup, alongside the other builtin-provider registrations.
pub fn register_builtin_providers() {
    register_provider(Arc::new(ProxmoxGuestExec));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_maps_guest_id_and_argv() {
        let op = op_from(
            &GuestRef {
                id: "100".into(),
                ..Default::default()
            },
            &ExecRequest {
                command: vec!["df".into(), "-h".into()],
                ..Default::default()
            },
        )
        .expect("numeric vmid maps");
        assert_eq!(op.vmid, 100);
        assert_eq!(op.argv, vec!["df".to_string(), "-h".to_string()]);
    }

    #[test]
    fn op_rejects_non_numeric_guest_id() {
        assert!(
            op_from(
                &GuestRef {
                    id: "example".into(),
                    ..Default::default()
                },
                &ExecRequest::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn push_op_maps_guest_id_and_fields() {
        let op = push_op_from(
            &GuestRef {
                id: "100".into(),
                ..Default::default()
            },
            WriteFileRequest {
                path: "/etc/example.conf".into(),
                contents: b"hi".to_vec(),
                mode: Some("0640".into()),
                owner: Some("root:root".into()),
            },
        )
        .expect("numeric vmid maps");
        assert_eq!(op.vmid, 100);
        assert_eq!(op.path, "/etc/example.conf");
        assert_eq!(op.contents, b"hi".to_vec());
        assert_eq!(op.mode.as_deref(), Some("0640"));
        assert_eq!(op.owner.as_deref(), Some("root:root"));
    }

    #[test]
    fn push_op_rejects_non_numeric_guest_id() {
        assert!(
            push_op_from(
                &GuestRef {
                    id: "example".into(),
                    ..Default::default()
                },
                WriteFileRequest {
                    path: "/etc/example.conf".into(),
                    contents: b"x".to_vec(),
                    ..Default::default()
                },
            )
            .is_err()
        );
    }
}
