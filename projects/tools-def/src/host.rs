//! Host addressing tools (slice 3 of the host-addressing plan).
//!
//! Three OrcaTool defs:
//!   - `host.info` — snapshot of every addressing channel for this host.
//!   - `host.set` — write a manual override (display_name, fqdn, or a
//!     specific LAN/Tailscale value). Keys are allowlisted.
//!   - `host.refresh` — force a re-detect (LAN + Tailscale + manual rows).
//!
//! Migrated to the `#[orca_tool]` proc-macro as the proof-of-shape pilot.
//! The macro emits `OrcaToolDef` + `OrcaOp` + wasm method unconditionally
//! and the `OrcaTool::run` thunk + inventory registration under
//! `feature = "native"`. Args/Output types stay defined outside the
//! native gate so wasm builds keep their typed `OrcaClient` methods.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Args / Output types (wasm-safe, shared by every surface) ────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostChannel {
    pub key: String,
    pub value: String,
    pub source: String,
    pub detected_at: i64,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostInfoOutput {
    pub display_name: String,
    pub machine_id: String,
    pub channels: Vec<HostChannel>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostSetArgs {
    /// One of: display_name | fqdn | lan_v4 | lan_v6 | tailscale_v4 | tailscale_v6.
    pub key: String,
    pub value: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostSetOutput {
    pub key: String,
    pub value: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostRefreshOutput {
    pub channels: Vec<HostChannel>,
}

/// Keys allowed in `host.set`. Anything else is rejected so we don't
/// accidentally proxy arbitrary settings writes through this tool.
pub const ALLOWED_HOST_KEYS: &[&str] = &[
    "display_name",
    "fqdn",
    "lan_v4",
    "lan_v6",
    "tailscale_v4",
    "tailscale_v6",
];

// ── Native bodies + tool registrations ──────────────────────────────────────

#[cfg(feature = "native")]
mod native_support {
    use super::*;
    use anyhow::Result;
    use orca_db as db;

    impl From<db::host_addressing::HostAddressingRow> for HostChannel {
        fn from(r: db::host_addressing::HostAddressingRow) -> Self {
            Self {
                key: r.key,
                value: r.value,
                source: r.source,
                detected_at: r.detected_at,
            }
        }
    }

    /// Best-effort OS hostname read for the info snapshot. We mirror the
    /// `hostname` Command path used inside the daemon's host_identity init —
    /// the cached static there isn't reachable from this wasm-safe crate.
    pub(super) fn os_hostname() -> String {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Hook the server registers at startup so `host.refresh` can drive
    /// `host_identity::refresh_and_persist` without tools-def depending on
    /// the server crate.
    pub trait HostRefreshHook: Send + Sync {
        fn refresh(&self, conn: &db::Conn) -> Result<()>;
    }
}

#[cfg(feature = "native")]
pub use native_support::HostRefreshHook;

/// Local host snapshot: display name, machine_id, and every addressing channel.
#[orca_tool(domain = "host", verb = "info")]
async fn host_info(
    _args: EmptyArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HostInfoOutput> {
    let conn = orca_db::open_default()?;
    let channels: Vec<HostChannel> = orca_db::host_addressing::list_host_addressing(&conn)?
        .into_iter()
        .map(Into::into)
        .collect();
    let display_name = channels
        .iter()
        .find(|c| c.key == "display_name")
        .map(|c| c.value.clone())
        .unwrap_or_else(native_support::os_hostname);
    let machine_id = orca_utils::config::Config::load()
        .ok()
        .and_then(|c| std::fs::read_to_string(c.app_dir.join("machine_id")).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    Ok(HostInfoOutput {
        display_name,
        machine_id,
        channels,
    })
}

/// Write a manual host addressing override (display_name, fqdn, or a channel value).
#[orca_tool(domain = "host", verb = "set")]
async fn host_set(
    args: HostSetArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HostSetOutput> {
    if !ALLOWED_HOST_KEYS.contains(&args.key.as_str()) {
        anyhow::bail!(
            "host.set: key '{}' is not in the allowlist ({:?})",
            args.key,
            ALLOWED_HOST_KEYS
        );
    }
    let conn = orca_db::open_default()?;
    match args.key.as_str() {
        "display_name" => orca_db::settings::set(&conn, "host.display_name", &args.value)?,
        "fqdn" => orca_db::settings::set(&conn, "host.fqdn", &args.value)?,
        _ => orca_db::host_addressing::upsert_host_addressing(
            &conn,
            &args.key,
            &args.value,
            "manual",
        )?,
    }
    Ok(HostSetOutput {
        key: args.key,
        value: args.value,
    })
}

/// Re-detect every host addressing channel (LAN + Tailscale + settings overrides).
#[orca_tool(domain = "host", verb = "refresh")]
async fn host_refresh(
    _args: EmptyArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HostRefreshOutput> {
    let conn = orca_db::open_default()?;
    if let Ok(hook) = ctx.service::<std::sync::Arc<dyn HostRefreshHook + Send + Sync>>() {
        hook.refresh(&conn)?;
    }
    let channels = orca_db::host_addressing::list_host_addressing(&conn)?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(HostRefreshOutput { channels })
}
