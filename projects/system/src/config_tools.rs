//! Config store tools — CRUD over the host-owned config row store.
//!
//! Four canonical verbs (six-verb surface — no domain verb names):
//!   - `config.list`   — enumerate rows (optionally filtered by noun/host).
//!   - `config.detail` — fetch one row by noun+name.
//!   - `config.upsert` — create-or-replace a row owned by the local host
//!     (cross-host writes route via mesh once §3.3 lands).
//!   - `config.delete` — remove a row owned by the local host.
//!
//! Each `config_row` is owned by a system id. Only the owning system may
//! mutate it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Args / Output ────────────────────────────────────────────────

#[derive::snake_aliases]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
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
    /// Filter by owning system id.
    #[arg(long)]
    pub host: Option<String>,
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive::snake_aliases]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct ConfigGetOutput {
    pub row: Option<ConfigRowOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_csp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csp_restart_pending: Option<bool>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigSetArgs {
    pub noun: String,
    pub name: String,
    /// JSON payload for the row. Must be a valid JSON document.
    pub json: String,
    /// Owning system id. Defaults to this host. Must equal this host until
    /// cross-host routing lands (§3.3).
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConfigSetOutput {
    pub row: ConfigRowOut,
    pub created: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ConfigDeleteArgs {
    pub noun: String,
    pub name: String,
    /// Owning system id. Defaults to this host.
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
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

    /// This host's system id, which is what owns a config row.
    pub(super) fn local_system(_conn: &db::Conn) -> String {
        crate::host_identity::machine_id().to_string()
    }
}

fn is_csp_row(noun: &str, name: &str) -> bool {
    noun == crate::csp::CONFIG_NOUN && name == crate::csp::CONFIG_NAME
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
    let (effective_csp, csp_restart_pending) = if is_csp_row(&args.noun, &args.name) {
        let configured = crate::csp::compose(&[], &crate::csp::Relaxations::load(&conn));
        match crate::csp::enforced() {
            Some(enforced) => {
                let pending = enforced != configured;
                (Some(enforced), Some(pending))
            }
            None => (Some(configured), None),
        }
    } else {
        (None, None)
    };
    Ok(ConfigGetOutput {
        row,
        effective_csp,
        csp_restart_pending,
    })
}

/// [MUTATES STATE] Upsert a config row. Refuses to write rows owned by a different host
/// — cross-host writes route via the mesh once peer-tool dispatch
/// lands (§3.3).
#[orca_tool(domain = "config", verb = "upsert")]
async fn config_set(
    args: ConfigSetArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigSetOutput> {
    if is_csp_row(&args.noun, &args.name) {
        let caller = ctx.verified_caller().cloned().or_else(|| ctx.caller());
        let admin = caller.is_some_and(|c| dispatch::tool_roles::satisfies(&c.role, "admin"));
        anyhow::ensure!(admin, "config server/csp requires role 'admin'");
        crate::csp::Relaxations::parse(&args.json)
            .map_err(|e| anyhow::anyhow!("invalid server/csp: {e:#}"))?;
    }
    let json = if args.noun == "schedule" {
        let caller = ctx.verified_caller().cloned().or_else(|| ctx.caller());
        let caller = caller.ok_or_else(|| anyhow::anyhow!("config schedule requires a caller"))?;
        crate::scheduler::stamp_run_as(&args.json, &caller)?
    } else {
        args.json
    };
    let conn = db::open_default()?;
    let local = native_support::local_system(&conn);
    let owner = args.host.unwrap_or_else(|| local.clone());
    anyhow::ensure!(
        !is_csp_row(&args.noun, &args.name) || owner == local,
        "config server/csp can only be set on the local host"
    );
    let created =
        db::config_store::set(&conn, &local, &owner, &args.noun, &args.name, &json, "cli")?;
    let row = db::config_store::get(&conn, &args.noun, &args.name)?
        .ok_or_else(|| anyhow::anyhow!("row vanished after write"))?
        .into();
    Ok(ConfigSetOutput { row, created })
}

/// [MUTATES STATE] Delete a config row owned by the local host.
#[orca_tool(domain = "config", verb = "delete")]
async fn config_delete(
    args: ConfigDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ConfigDeleteOutput> {
    let conn = db::open_default()?;
    let local = native_support::local_system(&conn);
    let owner = args.host.unwrap_or_else(|| local.clone());
    let removed = db::config_store::delete(&conn, &local, &owner, &args.noun, &args.name, "cli")?;
    Ok(ConfigDeleteOutput { removed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(dir: &std::path::Path) -> contract::ToolCtx {
        use contract::config::{Config, Model};
        contract::ToolCtx::new(std::sync::Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.to_path_buf(),
            memory_root: dir.to_path_buf(),
            db_path: dir.join("ctx.db"),
            ports: Default::default(),
        }))
    }

    fn detail(noun: &str, name: &str, ctx: &contract::ToolCtx) -> ConfigGetOutput {
        run(config_get(
            ConfigGetArgs {
                noun: noun.into(),
                name: name.into(),
            },
            ctx,
        ))
        .unwrap()
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn detail_reports_enforced_csp_and_pending_restart() {
        let dir = tempfile::tempdir().unwrap();
        db::with_thread_db_path(&dir.path().join("cfg.db"), || {
            let ctx = test_ctx(dir.path());
            let strict = crate::csp::strict();
            let out = detail("server", "csp", &ctx);
            assert_eq!(out.effective_csp, Some(strict.clone()));
            assert_eq!(out.csp_restart_pending, None);
            assert_eq!(detail("service", "plex", &ctx).effective_csp, None);

            let conn = db::open_default().unwrap();
            conn.execute(
                "INSERT INTO config_rows (id, host_owner, noun, name, json, is_replica)
                 VALUES ('r1', 'other', 'server', 'csp', ?1, 1)",
                [r#"{"img-src":["https://replica.example"]}"#],
            )
            .unwrap();
            let local = crate::csp::Relaxations::load(&conn);
            assert!(local.img_src.is_empty(), "replica row must not apply");
            drop(conn);

            crate::csp::set_enforced(&strict);
            let conn = db::open_default().unwrap();
            db::config_store::set(
                &conn,
                "h",
                "h",
                "server",
                "csp",
                r#"{"img-src":["https://img.example"]}"#,
                "test",
            )
            .unwrap();
            let out = detail("server", "csp", &ctx);
            assert_eq!(out.effective_csp, Some(strict));
            assert_eq!(out.csp_restart_pending, Some(true));
        });
    }

    fn upsert(json: &str, ctx: &contract::ToolCtx) -> anyhow::Result<ConfigSetOutput> {
        run(config_set(
            ConfigSetArgs {
                noun: "server".into(),
                name: "csp".into(),
                json: json.into(),
                host: None,
            },
            ctx,
        ))
    }

    fn caller(role: &str) -> contract::CallerIdentity {
        contract::CallerIdentity {
            user_id: "u".into(),
            username: "u".into(),
            role: role.into(),
            can_mutate: true,
        }
    }

    #[test]
    fn upsert_server_csp_requires_admin_and_valid_relaxations() {
        let dir = tempfile::tempdir().unwrap();
        crate::host_identity::init(dir.path()).ok();
        db::with_thread_db_path(&dir.path().join("cfg.db"), || {
            let ok = r#"{"img-src":["https://img.example"]}"#;
            assert!(upsert(ok, &test_ctx(dir.path())).is_err());
            let user = test_ctx(dir.path()).with_auth(caller("user"));
            assert!(upsert(ok, &user).is_err());
            let admin = test_ctx(dir.path()).with_auth(caller("admin"));
            assert!(upsert(r#"{"script-src":["*"]}"#, &admin).is_err());
            assert!(upsert(r#"{"frame-ancestors":["*"]}"#, &admin).is_err());
            assert!(upsert(ok, &admin).is_ok());
            let mut verified_user = test_ctx(dir.path()).with_auth(caller("admin"));
            verified_user.set_verified_caller(Some(caller("user")));
            assert!(upsert(ok, &verified_user).is_err());
            let mut verified_admin = test_ctx(dir.path()).with_auth(caller("user"));
            verified_admin.set_verified_caller(Some(caller("admin")));
            assert!(upsert(ok, &verified_admin).is_ok());
            let remote = run(config_set(
                ConfigSetArgs {
                    noun: "server".into(),
                    name: "csp".into(),
                    json: ok.into(),
                    host: Some("other-host".into()),
                },
                &admin,
            ));
            assert!(remote.is_err());
        });
    }

    #[test]
    fn upsert_schedule_stamps_the_writing_caller_as_owner() {
        let dir = tempfile::tempdir().unwrap();
        crate::host_identity::init(dir.path()).ok();
        db::with_thread_db_path(&dir.path().join("cfg.db"), || {
            let spoofed = r#"{"job":"x.y","cron":"@hourly","run_as":{"user_id":"root","role":"super-admin"}}"#;
            let write = |ctx: &contract::ToolCtx| {
                run(config_set(
                    ConfigSetArgs {
                        noun: "schedule".into(),
                        name: "nightly".into(),
                        json: spoofed.into(),
                        host: None,
                    },
                    ctx,
                ))
            };
            assert!(write(&test_ctx(dir.path())).is_err());
            let mut ctx = test_ctx(dir.path()).with_auth(caller("super-admin"));
            ctx.set_verified_caller(Some(caller("user")));
            let row = write(&ctx).unwrap().row;
            #[derive(serde::Deserialize)]
            struct Owner {
                user_id: String,
                role: String,
            }
            #[derive(serde::Deserialize)]
            struct Stamped {
                run_as: Owner,
            }
            let stamped: Stamped = serde_json::from_str(&row.json).unwrap();
            assert_eq!(
                (
                    stamped.run_as.user_id.as_str(),
                    stamped.run_as.role.as_str()
                ),
                ("u", "user")
            );
        });
    }
}
