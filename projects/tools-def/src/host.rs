//! Host addressing tools (slice 3 of the host-addressing plan).
//!
//! Three OrcaTool defs:
//!   - `host.info` — snapshot of every addressing channel for this host.
//!   - `host.set` — write a manual override (display_name, fqdn, or a
//!     specific LAN/Tailscale value). Keys are allowlisted.
//!   - `host.refresh` — force a re-detect (LAN + Tailscale + manual rows).
//!
//! Migrated to the `#[orca_tool]` proc-macro as the proof-of-shape pilot.
//! The macro emits `OrcaToolDef` + `OrcaOp` unconditionally and the
//! `OrcaTool::run` thunk + inventory registration under `feature = "native"`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Args / Output types (shared by every surface) ────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostChannel {
    pub key: String,
    pub value: String,
    pub source: String,
    pub detected_at: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostInfoOutput {
    pub display_name: String,
    pub machine_id: String,
    pub channels: Vec<HostChannel>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostSetArgs {
    /// One of: display_name | fqdn | lan_v4 | lan_v6 | tailscale_v4 | tailscale_v6.
    pub key: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostSetOutput {
    pub key: String,
    pub value: String,
}

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
    /// the cached static there isn't reachable from this crate.
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

#[cfg(feature = "native")]
pub trait ProvideHostRefresh {
    fn host_refresh(&self) -> std::sync::Arc<dyn HostRefreshHook + Send + Sync>;
}

#[cfg(feature = "native")]
pub fn register_host_refresh(ctx: &mut orca_tool::ToolCtx, p: &impl ProvideHostRefresh) {
    ctx.register_service(p.host_refresh());
}

/// Local host snapshot: display name, machine_id, and every addressing channel.
#[orca_tool(domain = "system.host", verb = "detail", remote_ok = true)]
async fn host_detail(
    _args: EmptyArgs,
    _ctx: &orca_tool::ToolCtx,
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
#[orca_tool(domain = "system.host", verb = "set")]
async fn host_set(args: HostSetArgs, _ctx: &orca_tool::ToolCtx) -> anyhow::Result<HostSetOutput> {
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
#[orca_tool(domain = "system.host", verb = "refresh")]
async fn host_refresh(
    _args: EmptyArgs,
    ctx: &orca_tool::ToolCtx,
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

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::test_support::empty_ctx as make_ctx;
    use std::sync::Arc;

    #[test]
    fn host_channel_from_row_copies_fields() {
        let row = orca_db::host_addressing::HostAddressingRow {
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
    fn allowed_keys_cover_expected_channels() {
        for k in [
            "display_name",
            "fqdn",
            "lan_v4",
            "lan_v6",
            "tailscale_v4",
            "tailscale_v6",
        ] {
            assert!(ALLOWED_HOST_KEYS.contains(&k), "missing {k}");
        }
    }

    #[test]
    fn os_hostname_returns_non_empty() {
        // The detect path shells out to `hostname`; on any sane test host this
        // returns a non-empty string. Falls back to "unknown" if not.
        let h = native_support::os_hostname();
        assert!(!h.is_empty());
    }

    #[tokio::test]
    async fn host_info_uses_display_name_channel_when_present() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let ctx = make_ctx();
        orca_db::with_db_path(path.clone(), async move {
            let conn = orca_db::open_default().unwrap();
            orca_db::host_addressing::upsert_host_addressing(
                &conn,
                "display_name",
                "testbox",
                "manual",
            )
            .unwrap();
            orca_db::host_addressing::upsert_host_addressing(
                &conn,
                "lan_v4",
                "10.0.0.5",
                "autodetect",
            )
            .unwrap();
            drop(conn);

            let out = host_detail(EmptyArgs {}, &ctx).await.unwrap();
            assert_eq!(out.display_name, "testbox");
            assert_eq!(out.channels.len(), 2);
        })
        .await;
    }

    #[tokio::test]
    async fn host_info_falls_back_to_os_hostname_when_no_channel() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = make_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            let out = host_detail(EmptyArgs {}, &ctx).await.unwrap();
            assert!(!out.display_name.is_empty());
            assert_eq!(out.channels.len(), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn host_set_rejects_unknown_key() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = make_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            let res = host_set(
                HostSetArgs {
                    key: "bogus".into(),
                    value: "x".into(),
                },
                &ctx,
            )
            .await;
            let err = res.err().expect("unknown key should fail");
            assert!(err.to_string().contains("not in the allowlist"));
        })
        .await;
    }

    #[tokio::test]
    async fn host_set_writes_display_name_to_settings() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let ctx = make_ctx();
        orca_db::with_db_path(path.clone(), async move {
            let out = host_set(
                HostSetArgs {
                    key: "display_name".into(),
                    value: "alpha".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(out.key, "display_name");
            assert_eq!(out.value, "alpha");
            let conn = orca_db::open_default().unwrap();
            assert_eq!(
                orca_db::settings::get(&conn, "host.display_name").unwrap(),
                Some("alpha".to_string())
            );
        })
        .await;
    }

    #[tokio::test]
    async fn host_set_writes_fqdn_to_settings() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = make_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            host_set(
                HostSetArgs {
                    key: "fqdn".into(),
                    value: "alpha.example.com".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
            let conn = orca_db::open_default().unwrap();
            assert_eq!(
                orca_db::settings::get(&conn, "host.fqdn").unwrap(),
                Some("alpha.example.com".to_string())
            );
        })
        .await;
    }

    #[tokio::test]
    async fn host_set_writes_channel_value_to_host_addressing() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = make_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            host_set(
                HostSetArgs {
                    key: "lan_v4".into(),
                    value: "10.0.0.7".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
            let conn = orca_db::open_default().unwrap();
            let rows = orca_db::host_addressing::list_host_addressing(&conn).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].key, "lan_v4");
            assert_eq!(rows[0].value, "10.0.0.7");
            assert_eq!(rows[0].source, "manual");
        })
        .await;
    }

    #[tokio::test]
    async fn host_refresh_without_hook_returns_existing_channels() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = make_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            let conn = orca_db::open_default().unwrap();
            orca_db::host_addressing::upsert_host_addressing(
                &conn,
                "lan_v4",
                "10.0.0.9",
                "autodetect",
            )
            .unwrap();
            drop(conn);

            let out = host_refresh(EmptyArgs {}, &ctx).await.unwrap();
            assert_eq!(out.channels.len(), 1);
            assert_eq!(out.channels[0].key, "lan_v4");
        })
        .await;
    }

    #[tokio::test]
    async fn host_refresh_invokes_registered_hook() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct CountingHook {
            called: Arc<AtomicBool>,
        }
        impl HostRefreshHook for CountingHook {
            fn refresh(&self, conn: &orca_db::Conn) -> anyhow::Result<()> {
                self.called.store(true, Ordering::SeqCst);
                orca_db::host_addressing::upsert_host_addressing(
                    conn,
                    "tailscale_v4",
                    "100.64.0.1",
                    "autodetect",
                )?;
                Ok(())
            }
        }

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let hook: Arc<dyn HostRefreshHook + Send + Sync> = Arc::new(CountingHook {
            called: called.clone(),
        });
        let mut ctx = make_ctx();
        ctx.register_service(hook);

        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            let out = host_refresh(EmptyArgs {}, &ctx).await.unwrap();
            assert!(called.load(Ordering::SeqCst));
            assert_eq!(out.channels.len(), 1);
            assert_eq!(out.channels[0].key, "tailscale_v4");
            assert_eq!(out.channels[0].value, "100.64.0.1");
        })
        .await;
    }
}
