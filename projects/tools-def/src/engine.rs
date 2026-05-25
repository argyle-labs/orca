//! Engine domain — LLM backend registry (LM Studio, Ollama).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Args ────────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Display name, e.g. "lmstudio-local".
    pub name: String,
    /// Base URL, e.g. "http://localhost:1234".
    pub url: String,
    /// Backend kind: "lmstudio" | "ollama". Inferred from port 11434 if empty.
    #[serde(default)]
    #[cfg_attr(feature = "cli", arg(default_value = ""))]
    pub kind: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NameArgs {
    /// Backend name.
    pub name: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateArgs {
    /// Backend name.
    pub name: String,
    /// true = enable for model discovery, false = disable without removing.
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProviderDto {
    pub name: String,
    pub url: String,
    pub kind: String,
    pub enabled: bool,
    pub created_at: String,
}

/// Newtype wrapping `Vec<ProviderDto>` so it crosses the WASM boundary with a
/// real TS array type (`ProviderDto[]`) instead of `any`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ProviderList(pub Vec<ProviderDto>);

/// Outcome of a mutation (add/remove/enable/disable).
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EngineOpResult {
    /// Human-readable summary of what happened.
    pub message: String,
}

// ── Native helpers ──────────────────────────────────────────────────────────

#[cfg(feature = "native")]
impl From<orca_db::llm::Provider> for ProviderDto {
    fn from(p: orca_db::llm::Provider) -> Self {
        Self {
            name: p.name,
            url: p.url,
            kind: p.kind,
            enabled: p.enabled,
            created_at: p.created_at,
        }
    }
}

#[cfg(feature = "native")]
fn infer_kind(url: &str, supplied: &str) -> anyhow::Result<String> {
    let kind = if supplied.is_empty() {
        if url.contains(":11434") {
            "ollama"
        } else {
            "lmstudio"
        }
        .to_string()
    } else {
        supplied.to_string()
    };
    match kind.as_str() {
        "ollama" | "lmstudio" => Ok(kind),
        other => anyhow::bail!("unknown backend kind '{other}' (want: ollama|lmstudio)"),
    }
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// List registered LLM backends (LM Studio, Ollama).
#[orca_tool(domain = "system.engine", verb = "list", cli = manual)]
async fn engine_list(_args: EmptyArgs, _ctx: &orca_tool::ToolCtx) -> anyhow::Result<ProviderList> {
    let conn = orca_db::open_default()?;
    Ok(ProviderList(
        orca_db::llm::list(&conn)?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

/// Register a new LLM backend. Kind auto-inferred from URL if not supplied.
#[orca_tool(domain = "system.engine", verb = "create", cli = manual)]
async fn engine_create(args: AddArgs, _ctx: &orca_tool::ToolCtx) -> anyhow::Result<EngineOpResult> {
    let conn = orca_db::open_default()?;
    let kind = infer_kind(&args.url, &args.kind)?;
    orca_db::llm::upsert(&conn, &args.name, &args.url, &kind)?;
    Ok(EngineOpResult {
        message: format!("registered {kind} {} ({})", args.name, args.url),
    })
}

/// Remove a registered LLM backend.
#[orca_tool(domain = "system.engine", verb = "delete", cli = manual)]
async fn engine_delete(
    args: NameArgs,
    _ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<EngineOpResult> {
    let conn = orca_db::open_default()?;
    if orca_db::llm::remove(&conn, &args.name)? {
        Ok(EngineOpResult {
            message: format!("removed {}", args.name),
        })
    } else {
        anyhow::bail!("no backend named '{}'", args.name)
    }
}

/// Enable or disable a backend without removing it.
#[orca_tool(domain = "system.engine", verb = "update", cli = manual)]
async fn engine_update(
    args: UpdateArgs,
    _ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<EngineOpResult> {
    let conn = orca_db::open_default()?;
    if orca_db::llm::set_enabled(&conn, &args.name, args.enabled)? {
        let state = if args.enabled { "enabled" } else { "disabled" };
        Ok(EngineOpResult {
            message: format!("{} {state}", args.name),
        })
    } else {
        anyhow::bail!("no backend named '{}'", args.name)
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::test_support::empty_ctx;

    #[test]
    fn infer_kind_uses_supplied_value() {
        assert_eq!(infer_kind("http://x", "ollama").unwrap(), "ollama");
        assert_eq!(infer_kind("http://x", "lmstudio").unwrap(), "lmstudio");
    }

    #[test]
    fn infer_kind_defaults_to_lmstudio() {
        assert_eq!(infer_kind("http://localhost:1234", "").unwrap(), "lmstudio");
    }

    #[test]
    fn infer_kind_defaults_to_ollama_on_11434() {
        assert_eq!(infer_kind("http://localhost:11434", "").unwrap(), "ollama");
    }

    #[test]
    fn infer_kind_rejects_unknown() {
        let e = infer_kind("http://x", "bogus").unwrap_err();
        assert!(e.to_string().contains("unknown backend kind"));
    }

    #[test]
    fn provider_dto_from_db_row_copies_fields() {
        let dto: ProviderDto = orca_db::llm::Provider {
            name: "n".into(),
            url: "u".into(),
            kind: "ollama".into(),
            enabled: true,
            created_at: "ts".into(),
        }
        .into();
        assert_eq!(dto.name, "n");
        assert_eq!(dto.url, "u");
        assert_eq!(dto.kind, "ollama");
        assert!(dto.enabled);
        assert_eq!(dto.created_at, "ts");
    }

    #[tokio::test]
    async fn engine_lifecycle_add_list_disable_remove() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            // empty initially
            let list0 = engine_list(EmptyArgs {}, &ctx).await.unwrap();
            assert!(list0.0.is_empty());

            // add
            let r = engine_create(
                AddArgs {
                    name: "local".into(),
                    url: "http://localhost:1234".into(),
                    kind: String::new(),
                },
                &ctx,
            )
            .await
            .unwrap();
            assert!(r.message.contains("lmstudio"));

            let list1 = engine_list(EmptyArgs {}, &ctx).await.unwrap();
            assert_eq!(list1.0.len(), 1);
            assert!(list1.0[0].enabled);

            // disable
            let d = engine_update(
                UpdateArgs {
                    name: "local".into(),
                    enabled: false,
                },
                &ctx,
            )
            .await
            .unwrap();
            assert!(d.message.contains("disabled"));
            let list2 = engine_list(EmptyArgs {}, &ctx).await.unwrap();
            assert!(!list2.0[0].enabled);

            // enable
            engine_update(
                UpdateArgs {
                    name: "local".into(),
                    enabled: true,
                },
                &ctx,
            )
            .await
            .unwrap();
            let list3 = engine_list(EmptyArgs {}, &ctx).await.unwrap();
            assert!(list3.0[0].enabled);

            // remove
            engine_delete(
                NameArgs {
                    name: "local".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
            let list4 = engine_list(EmptyArgs {}, &ctx).await.unwrap();
            assert!(list4.0.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn engine_delete_unknown_errors() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            let e = engine_delete(
                NameArgs {
                    name: "ghost".into(),
                },
                &ctx,
            )
            .await
            .err()
            .unwrap();
            assert!(e.to_string().contains("no backend named"));
        })
        .await;
    }

    #[tokio::test]
    async fn engine_enable_unknown_errors() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            assert!(
                engine_update(
                    UpdateArgs {
                        name: "ghost".into(),
                        enabled: true,
                    },
                    &ctx,
                )
                .await
                .is_err()
            );
            assert!(
                engine_update(
                    UpdateArgs {
                        name: "ghost".into(),
                        enabled: false,
                    },
                    &ctx,
                )
                .await
                .is_err()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn engine_create_rejects_unknown_kind() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let ctx = empty_ctx();
        orca_db::with_db_path(tmp.path().to_path_buf(), async move {
            let e = engine_create(
                AddArgs {
                    name: "x".into(),
                    url: "http://x".into(),
                    kind: "bogus".into(),
                },
                &ctx,
            )
            .await
            .err()
            .unwrap();
            assert!(e.to_string().contains("unknown"));
        })
        .await;
    }
}

// ── CLI registration — bespoke colored rendering ────────────────────────────
#[cfg(feature = "cli")]
mod cli_register {
    use super::*;
    use colored::Colorize;

    crate::register_op! {
        tool: EngineList,
        domain: "engine",
        verb: "list",
        summary: "List registered LLM backends",
        render: |out| {
            if out.0.is_empty() {
                println!("{}", "no LLM backends registered".dimmed());
                println!(
                    "{}",
                    "  use `orca engine create <name> <url> [lmstudio|ollama]` to add one".dimmed()
                );
                return Ok(());
            }
            for p in &out.0 {
                let status = if p.enabled {
                    "enabled".green().to_string()
                } else {
                    "disabled".dimmed().to_string()
                };
                println!("  {} {} {} ({})", p.name.bold(), p.kind.cyan(), p.url, status);
            }
        }
    }

    crate::register_op! {
        tool: EngineCreate,
        domain: "engine",
        verb: "create",
        summary: "Register an LLM backend",
        render: |out| { println!("{}", out.message); }
    }

    crate::register_op! {
        tool: EngineDelete,
        domain: "engine",
        verb: "delete",
        summary: "Remove a registered LLM backend",
        render: |out| { println!("{}", out.message); }
    }

    crate::register_op! {
        tool: EngineUpdate,
        domain: "engine",
        verb: "update",
        summary: "Enable or disable a backend without removing it",
        render: |out| { println!("{}", out.message); }
    }
}
