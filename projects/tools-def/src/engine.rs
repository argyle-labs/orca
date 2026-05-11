//! Engine domain — LLM backend registry (LM Studio, Ollama).
//!
//! Single source of truth: Args + unit structs + `OrcaToolDef` impls are
//! always-compiled (wasm-safe). The `OrcaTool::run` impls below are gated on
//! the `native` feature so wasm builds skip the db/utils deps entirely.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Args ────────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Display name, e.g. "lmstudio-local".
    pub name: String,
    /// Base URL, e.g. "http://localhost:1234".
    pub url: String,
    /// Backend kind: "lmstudio" | "ollama". Inferred from port 11434 if empty.
    #[serde(default)]
    pub kind: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NameArgs {
    /// Backend name.
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ProviderList(pub Vec<ProviderDto>);

/// Outcome of a mutation (add/remove/enable/disable).
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EngineOpResult {
    /// Human-readable summary of what happened.
    pub message: String,
}

// ── Tool unit structs + OrcaToolDef impls ───────────────────────────────────

pub struct EngineList;
impl OrcaToolDef for EngineList {
    const NAME: &'static str = "engine.list";
    const DESCRIPTION: &'static str = "List registered LLM backends (LM Studio, Ollama).";
    type Args = EmptyArgs;
    type Output = ProviderList;
}

pub struct EngineAdd;
impl OrcaToolDef for EngineAdd {
    const NAME: &'static str = "engine.add";
    const DESCRIPTION: &'static str =
        "Register a new LLM backend. Kind auto-inferred from URL if not supplied.";
    type Args = AddArgs;
    type Output = EngineOpResult;
}

pub struct EngineRemove;
impl OrcaToolDef for EngineRemove {
    const NAME: &'static str = "engine.remove";
    const DESCRIPTION: &'static str = "Remove a registered LLM backend.";
    type Args = NameArgs;
    type Output = EngineOpResult;
}

pub struct EngineEnable;
impl OrcaToolDef for EngineEnable {
    const NAME: &'static str = "engine.enable";
    const DESCRIPTION: &'static str = "Enable a backend for model discovery.";
    type Args = NameArgs;
    type Output = EngineOpResult;
}

pub struct EngineDisable;
impl OrcaToolDef for EngineDisable {
    const NAME: &'static str = "engine.disable";
    const DESCRIPTION: &'static str = "Disable a backend without removing it.";
    type Args = NameArgs;
    type Output = EngineOpResult;
}

// ── Native run impls ────────────────────────────────────────────────────────

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_db as db;
    use orca_utils::tool::{OrcaTool, ToolCtx};

    impl From<db::llm::Provider> for ProviderDto {
        fn from(p: db::llm::Provider) -> Self {
            Self {
                name: p.name,
                url: p.url,
                kind: p.kind,
                enabled: p.enabled,
                created_at: p.created_at,
            }
        }
    }

    fn infer_kind(url: &str, supplied: &str) -> Result<String> {
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

    #[async_trait]
    impl OrcaTool for EngineList {
        async fn run(_args: EmptyArgs, _ctx: &ToolCtx) -> Result<ProviderList> {
            let conn = db::open_default()?;
            Ok(ProviderList(
                db::llm::list(&conn)?.into_iter().map(Into::into).collect(),
            ))
        }
    }

    #[async_trait]
    impl OrcaTool for EngineAdd {
        async fn run(args: AddArgs, _ctx: &ToolCtx) -> Result<EngineOpResult> {
            let conn = db::open_default()?;
            let kind = infer_kind(&args.url, &args.kind)?;
            db::llm::upsert(&conn, &args.name, &args.url, &kind)?;
            Ok(EngineOpResult {
                message: format!("registered {kind} {} ({})", args.name, args.url),
            })
        }
    }

    #[async_trait]
    impl OrcaTool for EngineRemove {
        async fn run(args: NameArgs, _ctx: &ToolCtx) -> Result<EngineOpResult> {
            let conn = db::open_default()?;
            if db::llm::remove(&conn, &args.name)? {
                Ok(EngineOpResult {
                    message: format!("removed {}", args.name),
                })
            } else {
                anyhow::bail!("no backend named '{}'", args.name)
            }
        }
    }

    #[async_trait]
    impl OrcaTool for EngineEnable {
        async fn run(args: NameArgs, _ctx: &ToolCtx) -> Result<EngineOpResult> {
            let conn = db::open_default()?;
            if db::llm::set_enabled(&conn, &args.name, true)? {
                Ok(EngineOpResult {
                    message: format!("{} enabled", args.name),
                })
            } else {
                anyhow::bail!("no backend named '{}'", args.name)
            }
        }
    }

    #[async_trait]
    impl OrcaTool for EngineDisable {
        async fn run(args: NameArgs, _ctx: &ToolCtx) -> Result<EngineOpResult> {
            let conn = db::open_default()?;
            if db::llm::set_enabled(&conn, &args.name, false)? {
                Ok(EngineOpResult {
                    message: format!("{} disabled", args.name),
                })
            } else {
                anyhow::bail!("no backend named '{}'", args.name)
            }
        }
    }
}
