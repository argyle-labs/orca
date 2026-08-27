//! Config store tools — CRUD over the host-owned config row store.
//!
//! Four canonical verbs (six-verb surface — no domain verb names):
//!   - `config.list`   — enumerate rows (optionally filtered by noun/host).
//!   - `config.detail` — fetch one row by noun+name.
//!   - `config.upsert` — create-or-replace a row owned by the local host
//!     (cross-host writes route via mesh once §3.3 lands).
//!   - `config.delete` — remove a row owned by the local host.
//!
//! Each `config_row` carries a `host_owner`. Only the owning host may
//! mutate.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Args / Output ────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ConfigRowOut {
    pub id: String,
    pub host_owner: String,
    pub noun: String,
    pub name: String,
    pub json: String,
    pub is_replica: bool,
    pub updated_at: String,
    pub updated_by: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct ConfigListArgs {
    /// Filter by noun (service, schedule, backup_job, nfs_watch, …).
    #[arg(long)]
    pub noun: Option<String>,
    /// Filter by host_owner.
    #[arg(long)]
    pub host: Option<String>,
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigListOutput {
    pub rows: Vec<ConfigRowOut>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigGetArgs {
    /// Row noun (service, schedule, backup_job, …).
    pub noun: String,
    /// Row name (e.g. "plex", "host.backup").
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigGetOutput {
    pub row: Option<ConfigRowOut>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigSetArgs {
    pub noun: String,
    pub name: String,
    /// JSON payload for the row. Must be a valid JSON document.
    pub json: String,
    /// host_owner. Defaults to the local host's display_name. Must equal
    /// the local host until cross-host routing lands (§3.3).
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ConfigSetOutput {
    pub row: ConfigRowOut,
    pub created: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigDeleteArgs {
    pub noun: String,
    pub name: String,
    /// host_owner. Defaults to the local host's display_name.
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ConfigDeleteOutput {
    pub removed: bool,
}

// ── Native support ───────────────────────────────────────────────────────────

mod native_support {
    use super::*;

    impl From<db::config_store::ConfigRow> for ConfigRowOut {
        fn from(r: db::config_store::ConfigRow) -> Self {
            ConfigRowOut {
                id: r.id,
                host_owner: r.host_owner,
                noun: r.noun,
                name: r.name,
                json: r.json,
                is_replica: r.is_replica,
                updated_at: r.updated_at,
                updated_by: r.updated_by,
            }
        }
    }

    /// Resolve this host's canonical name for config-row ownership.
    /// Prefers the `host.display_name` setting (operator-set), falls back
    /// to the OS hostname. Mirrors what `host.info` reports.
    pub(super) fn local_host(conn: &db::Conn) -> String {
        db::settings::get(conn, "host.display_name")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unknown".to_string())
            })
    }
}

// ── Tools ────────────────────────────────────────────────────────────────────

/// List config rows. Optionally filter by noun and/or host_owner.
#[orca_tool(domain = "config", verb = "list")]
async fn config_list(
    args: ConfigListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigListOutput> {
    let conn = db::open_default()?;
    let rows: Vec<ConfigRowOut> =
        db::config_store::list(&conn, args.noun.as_deref(), args.host.as_deref())?
            .into_iter()
            .map(Into::into)
            .collect();
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(rows, &params);
    Ok(ConfigListOutput {
        rows: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

/// Fetch a single config row by noun+name.
#[orca_tool(domain = "config", verb = "detail")]
async fn config_get(
    args: ConfigGetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigGetOutput> {
    let conn = db::open_default()?;
    let row = db::config_store::get(&conn, &args.noun, &args.name)?.map(Into::into);
    Ok(ConfigGetOutput { row })
}

/// Upsert a config row. Refuses to write rows owned by a different host
/// — cross-host writes route via the pod mesh once peer-tool dispatch
/// lands (§3.3).
#[orca_tool(domain = "config", verb = "upsert")]
async fn config_set(
    args: ConfigSetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigSetOutput> {
    let conn = db::open_default()?;
    let local = native_support::local_host(&conn);
    let owner = args.host.unwrap_or_else(|| local.clone());
    let created = db::config_store::set(
        &conn, &local, &owner, &args.noun, &args.name, &args.json, "cli",
    )?;
    let row = db::config_store::get(&conn, &args.noun, &args.name)?
        .ok_or_else(|| anyhow::anyhow!("row vanished after write"))?
        .into();
    Ok(ConfigSetOutput { row, created })
}

/// Delete a config row owned by the local host.
#[orca_tool(domain = "config", verb = "delete")]
async fn config_delete(
    args: ConfigDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigDeleteOutput> {
    let conn = db::open_default()?;
    let local = native_support::local_host(&conn);
    let owner = args.host.unwrap_or_else(|| local.clone());
    let removed = db::config_store::delete(&conn, &local, &owner, &args.noun, &args.name, "cli")?;
    Ok(ConfigDeleteOutput { removed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        use std::sync::Arc;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orca-cfg-ctx-{}-{}", std::process::id(), n));
        contract::ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("cfg-test.db"),
            ports: Default::default(),
        }))
    }

    /// Bind a temp DB on this thread, seed a deterministic `host.display_name`
    /// so `local_host()` is "testhost" (never the real OS hostname), then drive
    /// an async closure to completion on a current-thread runtime inside the
    /// sync scope so the thread-local DB path stays bound.
    fn with_db_block<Fut, T>(f: impl FnOnce() -> Fut) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config-tools.db");
        db::with_thread_db_path(&path, || {
            let conn = db::open_default().expect("open temp db");
            db::settings::set(&conn, "host.display_name", "testhost").expect("seed host name");
            drop(conn);
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(f())
        })
    }

    #[test]
    fn upsert_creates_then_updates_and_detail_reflects_it() {
        with_db_block(|| async {
            let out = config_set(
                ConfigSetArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    json: r#"{"port":32400}"#.into(),
                    host: None,
                },
                &ctx(),
            )
            .await
            .expect("first upsert");
            assert!(out.created, "first write must report created=true");
            assert_eq!(out.row.host_owner, "testhost");
            assert_eq!(out.row.noun, "service");
            assert_eq!(out.row.name, "plex");
            assert_eq!(out.row.json, r#"{"port":32400}"#);
            assert!(!out.row.is_replica);

            // detail reflects the write.
            let got = config_get(
                ConfigGetArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                },
                &ctx(),
            )
            .await
            .expect("detail")
            .row
            .expect("row present");
            assert_eq!(got.json, r#"{"port":32400}"#);

            // Second write to the same key updates (created=false).
            let out2 = config_set(
                ConfigSetArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    json: r#"{"port":32401}"#.into(),
                    host: None,
                },
                &ctx(),
            )
            .await
            .expect("second upsert");
            assert!(
                !out2.created,
                "re-write of same key must report created=false"
            );
            assert_eq!(out2.row.json, r#"{"port":32401}"#);
        });
    }

    #[test]
    fn detail_missing_row_is_none() {
        with_db_block(|| async {
            let got = config_get(
                ConfigGetArgs {
                    noun: "service".into(),
                    name: "nope".into(),
                },
                &ctx(),
            )
            .await
            .expect("detail ok");
            assert!(got.row.is_none());
        });
    }

    #[test]
    fn upsert_foreign_owner_is_refused() {
        with_db_block(|| async {
            let err = config_set(
                ConfigSetArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    json: "{}".into(),
                    host: Some("otherhost".into()),
                },
                &ctx(),
            )
            .await
            .expect_err("cross-host write must be refused");
            assert!(
                err.to_string()
                    .contains("refusing to write config row owned by 'otherhost'"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn upsert_invalid_json_errors() {
        with_db_block(|| async {
            let err = config_set(
                ConfigSetArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    json: "not json".into(),
                    host: None,
                },
                &ctx(),
            )
            .await
            .expect_err("invalid JSON payload must error");
            assert!(err.to_string().contains("not valid JSON"), "got: {err}");
        });
    }

    #[test]
    fn list_filters_by_noun_and_host_and_pages() {
        with_db_block(|| async {
            for (noun, name) in [
                ("service", "plex"),
                ("service", "sonarr"),
                ("schedule", "nightly"),
            ] {
                config_set(
                    ConfigSetArgs {
                        noun: noun.into(),
                        name: name.into(),
                        json: "{}".into(),
                        host: None,
                    },
                    &ctx(),
                )
                .await
                .expect("seed");
            }

            // No filter → all three rows.
            let all = config_list(ConfigListArgs::default(), &ctx())
                .await
                .expect("list all");
            assert_eq!(all.rows.len(), 3);
            assert_eq!(all.total, Some(3));
            assert!(all.next_cursor.is_none());

            // Filter by noun.
            let svc = config_list(
                ConfigListArgs {
                    noun: Some("service".into()),
                    ..Default::default()
                },
                &ctx(),
            )
            .await
            .expect("list service");
            assert_eq!(svc.rows.len(), 2);
            assert!(svc.rows.iter().all(|r| r.noun == "service"));

            // Filter by host_owner that matches nothing.
            let none = config_list(
                ConfigListArgs {
                    host: Some("ghost".into()),
                    ..Default::default()
                },
                &ctx(),
            )
            .await
            .expect("list ghost host");
            assert!(none.rows.is_empty());

            // limit=1 yields a next_cursor (more pages remain).
            let page1 = config_list(
                ConfigListArgs {
                    limit: Some(1),
                    ..Default::default()
                },
                &ctx(),
            )
            .await
            .expect("list page 1");
            assert_eq!(page1.rows.len(), 1);
            assert!(page1.next_cursor.is_some());
            assert_eq!(page1.total, Some(3));
        });
    }

    #[test]
    fn delete_removes_then_reports_absent() {
        with_db_block(|| async {
            config_set(
                ConfigSetArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    json: "{}".into(),
                    host: None,
                },
                &ctx(),
            )
            .await
            .expect("seed");

            let d1 = config_delete(
                ConfigDeleteArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    host: None,
                },
                &ctx(),
            )
            .await
            .expect("delete existing");
            assert!(d1.removed, "deleting an existing row reports removed=true");

            let d2 = config_delete(
                ConfigDeleteArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    host: None,
                },
                &ctx(),
            )
            .await
            .expect("delete again");
            assert!(!d2.removed, "deleting an absent row reports removed=false");
        });
    }

    #[test]
    fn delete_foreign_owner_is_refused() {
        with_db_block(|| async {
            let err = config_delete(
                ConfigDeleteArgs {
                    noun: "service".into(),
                    name: "plex".into(),
                    host: Some("otherhost".into()),
                },
                &ctx(),
            )
            .await
            .expect_err("cross-host delete must be refused");
            assert!(
                err.to_string()
                    .contains("refusing to delete config row owned by 'otherhost'"),
                "got: {err}"
            );
        });
    }
}
