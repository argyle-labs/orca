//! Host addressing primitives.
//!
//! Reads (display_name / machine_id / addressing channels) surface as
//! fields on `system.detail`. Writes (hostname, fqdn, lan_v4, lan_v6,
//! tailscale_v4, tailscale_v6, force-refresh) are handled by
//! `system.update`. There is no `system.host.*` orca_tool — host
//! information is a detail of the system, not a separate resource.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct HostChannel {
    pub key: String,
    pub value: String,
    pub source: String,
    pub detected_at: i64,
}

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

/// Best-effort OS hostname read. Used by `system.detail` to fill
/// `display_name` when no `display_name` channel has been set.
pub(crate) fn os_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Hook the server registers at startup so `system.update --refresh-host`
/// can drive `host_identity::refresh_and_persist` without this domain
/// crate depending on the server crate.
pub trait HostRefreshHook: Send + Sync {
    fn refresh(&self, conn: &db::Conn) -> anyhow::Result<()>;
}

pub trait ProvideHostRefresh {
    fn host_refresh(&self) -> std::sync::Arc<dyn HostRefreshHook + Send + Sync>;
}

pub fn register_host_refresh(ctx: &mut contract::ToolCtx, p: &impl ProvideHostRefresh) {
    ctx.register_service(p.host_refresh());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_channel_from_row_copies_fields() {
        let row = db::host_addressing::HostAddressingRow {
            key: "lan_v4".to_string(),
            value: "10.0.0.1".to_string(),
            source: "manual".to_string(),
            detected_at: 42,
        };
        let ch: HostChannel = row.into();
        assert_eq!(ch.key, "lan_v4");
        assert_eq!(ch.value, "10.0.0.1");
        assert_eq!(ch.source, "manual");
        assert_eq!(ch.detected_at, 42);
    }

    #[test]
    fn os_hostname_returns_non_empty() {
        let h = os_hostname();
        assert!(!h.is_empty());
    }
}
