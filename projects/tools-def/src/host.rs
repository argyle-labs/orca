//! Host addressing tools (slice 3 of the host-addressing plan).
//!
//! Three OrcaTool defs:
//!   - `host.info` — snapshot of every addressing channel for this host.
//!   - `host.set` — write a manual override (display_name, fqdn, or a
//!     specific LAN/Tailscale value). Keys are allowlisted.
//!   - `host.refresh` — force a re-detect (LAN + Tailscale + manual rows).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

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

pub struct HostInfo;
impl OrcaToolDef for HostInfo {
    const NAME: &'static str = "host.info";
    const DESCRIPTION: &'static str =
        "Local host snapshot: display name, machine_id, and every addressing channel.";
    type Args = EmptyArgs;
    type Output = HostInfoOutput;
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

pub struct HostSet;
impl OrcaToolDef for HostSet {
    const NAME: &'static str = "host.set";
    const DESCRIPTION: &'static str =
        "Write a manual host addressing override (display_name, fqdn, or a channel value).";
    type Args = HostSetArgs;
    type Output = HostSetOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostRefreshOutput {
    pub channels: Vec<HostChannel>,
}

pub struct HostRefresh;
impl OrcaToolDef for HostRefresh {
    const NAME: &'static str = "host.refresh";
    const DESCRIPTION: &'static str =
        "Re-detect every host addressing channel (LAN + Tailscale + settings overrides).";
    type Args = EmptyArgs;
    type Output = HostRefreshOutput;
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

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::{Result, bail};
    use async_trait::async_trait;
    use orca_db as db;
    use orca_utils::tool::{OrcaTool, ToolCtx};

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
    fn os_hostname() -> String {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }

    #[async_trait]
    impl OrcaTool for HostInfo {
        async fn run(_args: EmptyArgs, _ctx: &ToolCtx) -> Result<HostInfoOutput> {
            let conn = db::open_default()?;
            let channels: Vec<HostChannel> = db::host_addressing::list_host_addressing(&conn)?
                .into_iter()
                .map(Into::into)
                .collect();
            // Prefer the display_name row if present; OS hostname fallback.
            let display_name = channels
                .iter()
                .find(|c| c.key == "display_name")
                .map(|c| c.value.clone())
                .unwrap_or_else(os_hostname);
            // machine_id lives at <app_dir>/machine_id; read directly so
            // this crate stays free of the daemon's static cache.
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
    }

    #[async_trait]
    impl OrcaTool for HostSet {
        async fn run(args: HostSetArgs, _ctx: &ToolCtx) -> Result<HostSetOutput> {
            if !ALLOWED_HOST_KEYS.contains(&args.key.as_str()) {
                bail!(
                    "host.set: key '{}' is not in the allowlist ({:?})",
                    args.key,
                    ALLOWED_HOST_KEYS
                );
            }
            let conn = db::open_default()?;
            // display_name + fqdn route through settings so the autodetect
            // pass picks them up as 'manual' rows on next refresh.
            // Channel-specific values (lan_*, tailscale_*) write directly
            // into host_addressing.
            match args.key.as_str() {
                "display_name" => db::settings::set(&conn, "host.display_name", &args.value)?,
                "fqdn" => db::settings::set(&conn, "host.fqdn", &args.value)?,
                _ => db::host_addressing::upsert_host_addressing(
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
    }

    #[async_trait]
    impl OrcaTool for HostRefresh {
        async fn run(_args: EmptyArgs, ctx: &ToolCtx) -> Result<HostRefreshOutput> {
            // The refresh path is implemented in server's `host_identity`
            // so this crate stays wasm-safe (no if_addrs dep). The daemon
            // registers a `HostRefreshHook` service at startup; if it's
            // missing (running outside the daemon) we just list whatever
            // rows are already in the table.
            let conn = db::open_default()?;
            if let Ok(hook) = ctx.service::<std::sync::Arc<dyn HostRefreshHook + Send + Sync>>() {
                hook.refresh(&conn)?;
            }
            let channels = db::host_addressing::list_host_addressing(&conn)?
                .into_iter()
                .map(Into::into)
                .collect();
            Ok(HostRefreshOutput { channels })
        }
    }

    /// Hook the server registers at startup so `host.refresh` can drive
    /// `host_identity::refresh_and_persist` without tools-def depending on
    /// the server crate.
    pub trait HostRefreshHook: Send + Sync {
        fn refresh(&self, conn: &db::Conn) -> Result<()>;
    }
}

#[cfg(feature = "native")]
pub use native::HostRefreshHook;
