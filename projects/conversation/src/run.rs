//! `agent.run` tool — one-shot agent execution. Drives the resolver
//! (`llm::resolve`) and runs a `Session` or returns a delegation envelope.
//! Moved from `server::mcp::handlers::run`.

use crate::sessions::context::ProjectContext;
use crate::sessions::session::Session;
use anyhow::Result;
use contract::ToolCtx;
use derive::orca_tool;
use llm::buffer_sink;
use llm::resolve::{self, Resolution};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunArgs {
    /// Agent name (e.g. wolf, owl, fox, crow, raven, badger).
    #[arg(short, long, default_value = "wolf")]
    #[serde(default = "default_agent")]
    pub agent: String,
    /// Task or question to send to the agent.
    pub prompt: String,
}

fn default_agent() -> String {
    "wolf".to_string()
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct AgentRunOutput {
    /// Raw text returned by the local/server run, OR a JSON envelope
    /// `{action:"delegate_to_claude_code", agent, agent_prompt, task}`
    /// the caller must execute itself.
    pub output: String,
    /// True when the output is a delegation envelope rather than completed text.
    pub delegated: bool,
}

/// Delegate a task to an orca agent. Backend (local LLM / server-side Anthropic
/// / Claude Code delegation) is selected by `system.agent.backend.detail`.
/// Prefer deterministic tools (read_doc, search_docs, list_services, etc.)
/// over this — only use when the task genuinely needs language model reasoning.
#[orca_tool(domain = "agent", verb = "run")]
async fn agent_run(args: AgentRunArgs, ctx: &ToolCtx) -> Result<AgentRunOutput> {
    let config = &*ctx.config;
    let agent = args.agent.as_str();
    let prompt = args.prompt.as_str();

    let full_prompt = if agent != "wolf" && agent != "orca" {
        format!("Delegate this to @{agent}: {prompt}")
    } else {
        prompt.to_string()
    };

    let resolution = resolve::resolve(agent, config)?;

    match resolution {
        Resolution::Local(_) => {
            let out = run_session(&full_prompt, config, None).await?;
            Ok(AgentRunOutput {
                output: out,
                delegated: false,
            })
        }
        Resolution::LocalThenClaude(_) => match run_session(&full_prompt, config, None).await {
            Ok(out) => Ok(AgentRunOutput {
                output: out,
                delegated: false,
            }),
            Err(e) => {
                tracing::info!(
                    target: "agent_backend",
                    "local run for @{agent} failed ({e:#}); falling back to claude (claude mode)"
                );
                Ok(AgentRunOutput {
                    output: delegate_envelope(agent, prompt, config)?,
                    delegated: true,
                })
            }
        },
        Resolution::ServerClaude(m) => {
            let out = run_session(&full_prompt, config, Some(m)).await?;
            Ok(AgentRunOutput {
                output: out,
                delegated: false,
            })
        }
        Resolution::DelegateToClaudeCode => Ok(AgentRunOutput {
            output: delegate_envelope(agent, prompt, config)?,
            delegated: true,
        }),
    }
}

async fn run_session(
    full_prompt: &str,
    config: &contract::config::Config,
    forced_model: Option<contract::config::Model>,
) -> Result<String> {
    let (sink, buf) = buffer_sink();
    let pctx = ProjectContext::default();
    let mut session =
        Session::new_with_output_and_model(config.clone(), pctx, sink, forced_model).await?;
    session.one_shot(full_prompt.to_string()).await?;
    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner());
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn delegate_envelope(
    agent: &str,
    prompt: &str,
    config: &contract::config::Config,
) -> Result<String> {
    let agent_prompt = agents::resolve::load_agent_prompt(agent, config)
        .ok_or_else(|| anyhow::anyhow!("agent not found: {agent}"))?;
    let envelope = json!({
        "action": "delegate_to_claude_code",
        "agent": agent,
        "agent_prompt": agent_prompt,
        "task": prompt,
    });
    Ok(serde_json::to_string_pretty(&envelope)?)
}
