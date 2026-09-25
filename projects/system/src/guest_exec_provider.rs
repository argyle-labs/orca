//! Core-owned `guest_exec` provider backed by orca's privileged LXC seam.
//!
//! This is the in-tree counterpart to the plugin-loaded [`contract::guest_exec`]
//! providers: rather than a subprocess plugin, core registers a `"proxmox-lxc"`
//! provider at daemon startup that routes an [`ExecRequest`] straight to the
//! existing `orca admin lxc-exec` seam (via [`crate::lxc_exec::run_privileged_lxc`]).
//! The exec surface stays allowlisted by that seam — a request whose `argv[0]`
//! is not on [`crate::lxc_exec::ALLOWED_COMMANDS`] is refused root-side.
//!
//! `write_file` routes through a dedicated `pct push` seam (`orca admin
//! lxc-push`) — deliberately NOT the exec allowlist, so file bytes never force
//! allowlisting a shell/`tee`/`dd`.
//!
//! The name is `"proxmox-lxc"`, NOT `"proxmox"`: the proxmox plugin registers a
//! *different* transport (QEMU guest agent, VM-only) under `"proxmox"`, and the
//! registry replaces by name — sharing one name silently swaps transports.

use std::path::Path;
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

/// Registry name of this provider. Must stay distinct from the proxmox plugin's
/// `"proxmox"` VM/guest-agent backend — see the module docs.
pub const PROVIDER_NAME: &str = "proxmox-lxc";

/// Registry name of the proxmox plugin's QEMU-guest-agent backend (VM-only).
/// Named here only so [`provider_for_kind`] can route VMs to it.
const VM_PROVIDER_NAME: &str = "proxmox";

/// A [`GuestExec`] whose `exec` runs inside a Proxmox LXC through the scoped
/// `orca admin lxc-exec` sudoers seam. LXC-only: the VM/guest-agent path is the
/// proxmox plugin's `"proxmox"` provider; the trait shape is transport-agnostic.
struct ProxmoxGuestExec;

/// Map a unit `kind` to the guest-exec provider whose transport can reach it.
/// Unknown/absent kinds fall back to the in-tree LXC backend: it is always
/// present, so the caller gets a legible failure instead of "no such provider".
fn provider_for_kind(kind: Option<&str>) -> &'static str {
    match kind {
        Some("vm") | Some("qemu") => VM_PROVIDER_NAME,
        _ => PROVIDER_NAME,
    }
}

/// Resolve a guest id to its provider by asking the unit registry what kind of
/// unit it is. Best-effort: with no unit provider loaded (or an unknown id) this
/// yields `None` and the caller falls back to the in-tree LXC backend.
async fn kind_of(id: &str) -> Option<String> {
    contract::unit::all_units()
        .await
        .into_iter()
        .find(|u| u.id.id == id)
        .map(|u| u.id.kind)
}

/// Route a guest id to the guest-exec provider that can actually reach it.
async fn provider_for(id: &str) -> &'static str {
    provider_for_kind(kind_of(id).await.as_deref())
}

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
    /// Target guest id (a Proxmox vmid — LXC or VM; the backend is chosen from
    /// the unit registry's kind for this id).
    #[arg(long)]
    pub id: String,
    /// Command + args to run inside the guest. `argv[0]` must be on the
    /// lxc-exec allowlist or the seam refuses it.
    #[arg(long = "arg", value_name = "ARG")]
    pub argv: Vec<String>,
    /// Overall deadline in milliseconds. Omit for the provider default.
    #[arg(long)]
    pub timeout_ms: Option<u64>,
    /// PVE endpoint the guest lives in. Only the VM/guest-agent backend needs it.
    #[arg(long)]
    pub scope: Option<String>,
    /// PVE node within the scope. Only the VM/guest-agent backend needs it.
    #[arg(long)]
    pub node: Option<String>,
}

/// Run an allowlisted command inside a guest, returning its captured
/// stdout/stderr/exit code. Routes to the LXC seam or the VM guest-agent backend
/// by the guest's kind. Admin-only and side-effecting — mirrors the `orca admin
/// lxc-exec` privilege boundary.
#[orca_tool(domain = "guest", verb = "exec", data_mutation = true, role = "admin")]
async fn guest_exec_run(args: GuestExecArgs, _ctx: &ToolCtx) -> Result<ExecOutput> {
    let provider = provider_for(&args.id).await;
    let guest = GuestRef {
        scope: args.scope,
        node: args.node,
        id: args.id,
    };
    let req = ExecRequest {
        command: args.argv,
        timeout_ms: args.timeout_ms,
        ..Default::default()
    };
    contract::guest_exec::exec(provider, guest, req).await
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GuestWriteFileArgs {
    /// Target guest id (a Proxmox vmid — LXC or VM; the backend is chosen from
    /// the unit registry's kind for this id).
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
    /// PVE endpoint the guest lives in. Only the VM/guest-agent backend needs it.
    #[arg(long)]
    pub scope: Option<String>,
    /// PVE node within the scope. Only the VM/guest-agent backend needs it.
    #[arg(long)]
    pub node: Option<String>,
    /// Push the bytes even if they do not parse as the format `path`'s extension
    /// declares. The escape hatch for repairing an already-corrupt file with an
    /// intermediate state; every use is logged at WARN with the parse error.
    #[arg(long)]
    pub allow_unparseable: bool,
}

/// Refuse to plant a config a service cannot read. `/etc/jellyfin/network.xml` on
/// frigg sat unparseable from 2026-03-15 to 2026-09-24 — Jellyfin fell back to
/// defaults and silently discarded six months of real config while looking
/// healthy. Unknown extensions pass through untouched; `allow_unparseable` is the
/// logged opt-out for repairing an already-broken file.
fn check_contents_parse(path: &str, contents: &[u8], allow_unparseable: bool) -> Result<()> {
    let err = match utils::config_format::validate_for_path(Path::new(path), contents) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if allow_unparseable {
        tracing::warn!(path, error = %format!("{err:#}"), "guest.write_file forced past config validation");
        return Ok(());
    }
    Err(err.context(
        "pass --allow-unparseable to force the write (e.g. repairing an already-broken file)",
    ))
}

/// Write a file into a guest — the confined `pct push` seam for an LXC, the
/// guest-agent `file-write` for a VM. Admin-only and side-effecting — mirrors the
/// `orca admin lxc-push` privilege boundary; the payload never rides argv.
#[orca_tool(
    domain = "guest",
    verb = "write_file",
    data_mutation = true,
    role = "admin"
)]
async fn guest_write_file(args: GuestWriteFileArgs, _ctx: &ToolCtx) -> Result<()> {
    check_contents_parse(&args.path, args.contents.as_bytes(), args.allow_unparseable)?;
    let provider = provider_for(&args.id).await;
    let guest = GuestRef {
        scope: args.scope,
        node: args.node,
        id: args.id,
    };
    let req = WriteFileRequest {
        path: args.path,
        contents: args.contents.into_bytes(),
        mode: args.mode,
        owner: args.owner,
    };
    contract::guest_exec::write_file(provider, guest, req).await
}

/// Register the core-owned `proxmox-lxc` guest-exec provider. Called once at
/// daemon startup, *after* the capability probe: a host with no proxmox has no
/// `pct` seam, so advertising the backend there would only produce dead tools.
pub fn register_builtin_providers() {
    if !crate::capability::is_available("proxmox") {
        return;
    }
    register_provider(Arc::new(ProxmoxGuestExec));
}

#[cfg(test)]
mod tests {
    use super::*;

    // A duplicate registry name does not error — `register_provider` REPLACES by
    // name, so colliding with the plugin's `"proxmox"` VM backend silently swaps
    // this LXC transport out and `guest.exec` starts demanding `scope`/`node`.
    #[test]
    fn core_provider_name_does_not_collide_with_plugin() {
        assert_ne!(PROVIDER_NAME, "proxmox");
        assert_eq!(PROVIDER_NAME, "proxmox-lxc");
    }

    #[test]
    fn kind_routes_to_the_transport_that_can_reach_it() {
        assert_eq!(provider_for_kind(Some("lxc")), "proxmox-lxc");
        assert_eq!(provider_for_kind(Some("container")), "proxmox-lxc");
        assert_eq!(provider_for_kind(Some("vm")), "proxmox");
        assert_eq!(provider_for_kind(Some("qemu")), "proxmox");
        // Unknown/absent kind falls back to the always-present in-tree backend.
        assert_eq!(provider_for_kind(Some("tv_show")), "proxmox-lxc");
        assert_eq!(provider_for_kind(None), "proxmox-lxc");
    }

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

    // The exact frigg corruption: every attribute quote stripped by an
    // over-eager sed. IPs are 10.0.0.x — never real fleet addresses.
    const BROKEN_NETWORK_XML: &str = r#"<?xml version=1.0 encoding=utf-8?>
<NetworkConfiguration>
  <KnownProxies>
    <string>10.0.0.5</string>
  </KnownProxies>
</NetworkConfiguration>
"#;

    #[test]
    fn write_file_refuses_an_unparseable_config() {
        let err = check_contents_parse(
            "/etc/jellyfin/network.xml",
            BROKEN_NETWORK_XML.as_bytes(),
            false,
        )
        .expect_err("a config that does not parse must not reach the guest");
        let msg = format!("{err:#}");
        assert!(msg.contains("network.xml"), "names the file: {msg}");
        assert!(
            msg.contains("--allow-unparseable"),
            "points at the opt-out: {msg}"
        );
    }

    #[test]
    fn write_file_allows_a_valid_config_and_unknown_formats() {
        check_contents_parse(
            "/etc/jellyfin/network.xml",
            br#"<?xml version="1.0" encoding="utf-8"?><NetworkConfiguration/>"#,
            false,
        )
        .unwrap();
        // Opaque bytes must never be blocked just because we cannot name them.
        check_contents_parse("/etc/ssl/mesh.pem", b"-----BEGIN CERTIFICATE-----\n", false).unwrap();
    }

    #[test]
    fn the_escape_hatch_permits_a_forced_repair_write() {
        check_contents_parse(
            "/etc/jellyfin/network.xml",
            BROKEN_NETWORK_XML.as_bytes(),
            true,
        )
        .expect("--allow-unparseable forces the write");
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
