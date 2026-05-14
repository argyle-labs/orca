//! Engine domain — LLM backend registry (LM Studio, Ollama).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Args ────────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
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
#[orca_tool(domain = "engine", verb = "list", cli = manual)]
async fn engine_list(
    _args: EmptyArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProviderList> {
    let conn = orca_db::open_default()?;
    Ok(ProviderList(
        orca_db::llm::list(&conn)?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

/// Register a new LLM backend. Kind auto-inferred from URL if not supplied.
#[orca_tool(domain = "engine", verb = "add", cli = manual)]
async fn engine_add(
    args: AddArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<EngineOpResult> {
    let conn = orca_db::open_default()?;
    let kind = infer_kind(&args.url, &args.kind)?;
    orca_db::llm::upsert(&conn, &args.name, &args.url, &kind)?;
    Ok(EngineOpResult {
        message: format!("registered {kind} {} ({})", args.name, args.url),
    })
}

/// Remove a registered LLM backend.
#[orca_tool(domain = "engine", verb = "remove", cli = manual)]
async fn engine_remove(
    args: NameArgs,
    _ctx: &orca_utils::tool::ToolCtx,
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

/// Enable a backend for model discovery.
#[orca_tool(domain = "engine", verb = "enable", cli = manual)]
async fn engine_enable(
    args: NameArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<EngineOpResult> {
    let conn = orca_db::open_default()?;
    if orca_db::llm::set_enabled(&conn, &args.name, true)? {
        Ok(EngineOpResult {
            message: format!("{} enabled", args.name),
        })
    } else {
        anyhow::bail!("no backend named '{}'", args.name)
    }
}

/// Disable a backend without removing it.
#[orca_tool(domain = "engine", verb = "disable", cli = manual)]
async fn engine_disable(
    args: NameArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<EngineOpResult> {
    let conn = orca_db::open_default()?;
    if orca_db::llm::set_enabled(&conn, &args.name, false)? {
        Ok(EngineOpResult {
            message: format!("{} disabled", args.name),
        })
    } else {
        anyhow::bail!("no backend named '{}'", args.name)
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
                    "  use `orca engine add <name> <url> [lmstudio|ollama]` to add one".dimmed()
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
        tool: EngineAdd,
        domain: "engine",
        verb: "add",
        summary: "Register an LLM backend",
        render: |out| { println!("{}", out.message); }
    }

    crate::register_op! {
        tool: EngineRemove,
        domain: "engine",
        verb: "remove",
        summary: "Remove a registered LLM backend",
        render: |out| { println!("{}", out.message); }
    }

    crate::register_op! {
        tool: EngineEnable,
        domain: "engine",
        verb: "enable",
        summary: "Enable a backend for model discovery",
        render: |out| { println!("{}", out.message); }
    }

    crate::register_op! {
        tool: EngineDisable,
        domain: "engine",
        verb: "disable",
        summary: "Disable a backend without removing it",
        render: |out| { println!("{}", out.message); }
    }
}
