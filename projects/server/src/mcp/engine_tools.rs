//! Engine domain — LLM backend registry (LM Studio, Ollama).
//!
//! Five ops: list, add, remove, enable, disable. Each implements `OrcaTool`
//! so it lands on MCP + REST + CLI from one definition (proc-macro to
//! collapse the boilerplate is a follow-up).
//!
//! Naming convention (per surface-reorg plan):
//!   - MCP:  `engine.list`, `engine.add`, …
//!   - REST: `POST /api/ops/engine.list`, …  (universal exec mount)
//!   - CLI:  `orca engines <verb>` (existing) — thin shim dispatching to the
//!     same tool registry.

use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx, ToolRegistry};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Args / output shapes ─────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Display name, e.g. "lmstudio-local".
    pub name: String,
    /// Base URL, e.g. "http://localhost:1234".
    pub url: String,
    /// Backend kind: "lmstudio" | "ollama". Inferred from port 11434 if empty.
    #[serde(default)]
    pub kind: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct NameArgs {
    /// Backend name.
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub struct ProviderDto {
    pub name: String,
    pub url: String,
    pub kind: String,
    pub enabled: bool,
    pub created_at: String,
}

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

// ── Helpers ──────────────────────────────────────────────────────────────────

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

// ── Tools ────────────────────────────────────────────────────────────────────

pub struct EngineList;

#[async_trait]
impl OrcaTool for EngineList {
    const NAME: &'static str = "engine.list";
    const DESCRIPTION: &'static str = "List registered LLM backends (LM Studio, Ollama).";
    type Args = EmptyArgs;
    type Output = String;

    async fn run(_args: EmptyArgs, _ctx: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        let providers: Vec<ProviderDto> =
            db::llm::list(&conn)?.into_iter().map(Into::into).collect();
        Ok(serde_json::to_string(&providers)?)
    }
}

pub struct EngineAdd;

#[async_trait]
impl OrcaTool for EngineAdd {
    const NAME: &'static str = "engine.add";
    const DESCRIPTION: &'static str =
        "Register a new LLM backend. Kind auto-inferred from URL if not supplied.";
    type Args = AddArgs;
    type Output = String;

    async fn run(args: AddArgs, _ctx: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        let kind = infer_kind(&args.url, &args.kind)?;
        db::llm::upsert(&conn, &args.name, &args.url, &kind)?;
        Ok(format!("registered {kind} {} ({})", args.name, args.url))
    }
}

pub struct EngineRemove;

#[async_trait]
impl OrcaTool for EngineRemove {
    const NAME: &'static str = "engine.remove";
    const DESCRIPTION: &'static str = "Remove a registered LLM backend.";
    type Args = NameArgs;
    type Output = String;

    async fn run(args: NameArgs, _ctx: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::llm::remove(&conn, &args.name)? {
            Ok(format!("removed {}", args.name))
        } else {
            anyhow::bail!("no backend named '{}'", args.name)
        }
    }
}

pub struct EngineEnable;

#[async_trait]
impl OrcaTool for EngineEnable {
    const NAME: &'static str = "engine.enable";
    const DESCRIPTION: &'static str = "Enable a backend for model discovery.";
    type Args = NameArgs;
    type Output = String;

    async fn run(args: NameArgs, _ctx: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::llm::set_enabled(&conn, &args.name, true)? {
            Ok(format!("{} enabled", args.name))
        } else {
            anyhow::bail!("no backend named '{}'", args.name)
        }
    }
}

pub struct EngineDisable;

#[async_trait]
impl OrcaTool for EngineDisable {
    const NAME: &'static str = "engine.disable";
    const DESCRIPTION: &'static str = "Disable a backend without removing it.";
    type Args = NameArgs;
    type Output = String;

    async fn run(args: NameArgs, _ctx: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::llm::set_enabled(&conn, &args.name, false)? {
            Ok(format!("{} disabled", args.name))
        } else {
            anyhow::bail!("no backend named '{}'", args.name)
        }
    }
}

pub fn register(reg: &mut ToolRegistry) {
    reg.register::<EngineList>()
        .register::<EngineAdd>()
        .register::<EngineRemove>()
        .register::<EngineEnable>()
        .register::<EngineDisable>();
}
