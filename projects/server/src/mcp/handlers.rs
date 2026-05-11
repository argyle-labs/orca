use crate::llm::buffer_sink;
use anyhow::Result;
use orca_utils::config::Config;
use serde_json::{Value, json};

use crate::agent_backend::{self, Resolution};
use crate::context::ProjectContext;
use crate::conversation::session::Session;

pub async fn run(args: &Value, config: &Config) -> Result<String> {
    let agent = args["agent"].as_str().unwrap_or("wolf");
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;

    let full_prompt = if agent != "wolf" && agent != "orca" {
        format!("Delegate this to @{agent}: {prompt}")
    } else {
        prompt.to_string()
    };

    let resolution = agent_backend::resolve(agent, config)?;

    match resolution {
        Resolution::Local(_) => {
            // Try LM Studio first. If anything goes wrong (server unreachable,
            // no model loaded, mid-call error) fall back to delegating to
            // Claude Code so the user's task continues instead of dying.
            match run_session(agent, &full_prompt, config, None).await {
                Ok(out) => Ok(out),
                Err(e) => {
                    tracing::warn!(
                        target: "agent_backend",
                        "local run for @{agent} failed ({e:#}); falling back to claude code"
                    );
                    delegate_envelope(agent, prompt, config)
                }
            }
        }
        Resolution::ServerClaude(m) => run_session(agent, &full_prompt, config, Some(m)).await,
        Resolution::DelegateToClaudeCode => delegate_envelope(agent, prompt, config),
    }
}

async fn run_session(
    _agent: &str,
    full_prompt: &str,
    config: &Config,
    forced_model: Option<orca_utils::config::Model>,
) -> Result<String> {
    let (sink, buf) = buffer_sink();
    let ctx = ProjectContext::default();
    let mut session =
        Session::new_with_output_and_model(config.clone(), ctx, sink, forced_model).await?;
    session.one_shot(full_prompt.to_string()).await?;
    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner());
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Build the structured envelope the caller (a Claude Code session) consumes
/// to run the agent itself via `get_agent` + `Agent(general-purpose)`.
fn delegate_envelope(agent: &str, prompt: &str, config: &Config) -> Result<String> {
    let agent_prompt = crate::mcp::agent_resolve::load_agent_prompt(agent, config)
        .ok_or_else(|| anyhow::anyhow!("agent not found: {agent}"))?;
    let envelope = json!({
        "action": "delegate_to_claude_code",
        "agent": agent,
        "agent_prompt": agent_prompt,
        "task": prompt,
    });
    Ok(serde_json::to_string_pretty(&envelope)?)
}
