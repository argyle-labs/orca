//! Resolve which model handles an agent invocation. Driven by the
//! installed-model registry (`db::models`) + a per-agent pin stored
//! under the `settings` key `agent.<name>.model_id`. No global "mode",
//! no silent fallbacks — exactly one resolution per (agent, call).

use crate::discovery::{TaskKind, discover_all, select_for_task, to_config_model};
use anyhow::{Context, Result};
use contract::config::{Config, Model};
use db;

const KEY_AGENT_MODEL_PREFIX: &str = "agent.";
const KEY_AGENT_MODEL_SUFFIX: &str = ".model_id";

/// Resolution outcome — what the caller (run_agent) should do.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Run the agent locally (LM Studio / Ollama). Hard-fail if the
    /// endpoint is unreachable — no fallback.
    Local(Model),
    /// Run against the Anthropic API server-side. Only emitted when an
    /// API key is stored for the chosen model row.
    ServerClaude(Model),
    /// Tell the caller (a Claude Code session) to run the agent itself.
    /// Emitted for the `claude-code` provider.
    DelegateToClaudeCode,
}

fn agent_pin_key(agent: &str) -> String {
    format!("{KEY_AGENT_MODEL_PREFIX}{agent}{KEY_AGENT_MODEL_SUFFIX}")
}

/// Set / clear the model an agent uses. `None` clears the pin.
pub fn set_agent_model(agent: &str, model_id: Option<&str>) -> Result<()> {
    let conn = db::open_default()?;
    let key = agent_pin_key(agent);
    match model_id {
        Some(id) => db::settings::set(&conn, &key, id),
        None => db::settings::delete(&conn, &key).map(|_| ()),
    }
}

pub fn get_agent_model(agent: &str) -> Result<Option<String>> {
    let conn = db::open_default()?;
    db::settings::get(&conn, &agent_pin_key(agent))
}

/// Look up the model row that should handle this agent. Per-agent pin
/// wins; otherwise the global `is_default` row.
pub fn lookup_model_for_agent(agent: &str) -> Result<db::models::Model> {
    let conn = db::open_default()?;
    if let Some(pin) = db::settings::get(&conn, &agent_pin_key(agent))? {
        return db::models::get(&conn, &pin)?.with_context(|| {
            format!("agent '{agent}' pinned to model '{pin}' but that model does not exist")
        });
    }
    db::models::default(&conn)?
        .context("no default model installed — run `model.create --is-default ...` to install one")
}

/// Decide how to dispatch an agent invocation.
pub fn resolve(agent: &str, _config: &Config) -> Result<Resolution> {
    let row = lookup_model_for_agent(agent)?;
    if !row.enabled {
        anyhow::bail!(
            "model '{}' is disabled — enable it or pin a different model",
            row.id
        );
    }
    decide(&row)
}

/// Pure dispatch — reads only the supplied row (and the secret for the
/// anthropic case). Exposed for tests.
pub fn decide(row: &db::models::Model) -> Result<Resolution> {
    match row.provider.as_str() {
        "lmstudio" => {
            let url = row
                .endpoint
                .clone()
                .ok_or_else(|| anyhow::anyhow!("model '{}' missing endpoint", row.id))?;
            Ok(Resolution::Local(Model::LMStudio {
                id: row.model_name.clone(),
                url,
            }))
        }
        "ollama" => {
            let url = row
                .endpoint
                .clone()
                .ok_or_else(|| anyhow::anyhow!("model '{}' missing endpoint", row.id))?;
            Ok(Resolution::Local(Model::Ollama {
                id: row.model_name.clone(),
                url,
            }))
        }
        "anthropic" => {
            let conn = db::open_default()?;
            let key = db::settings::secret_get(&conn, &format!("model.{}.api_key", row.id))?;
            if key.is_none() {
                anyhow::bail!(
                    "model '{}' (anthropic) has no API key — store one with `model.update --api-key …`",
                    row.id
                );
            }
            Ok(Resolution::ServerClaude(Model::Claude(
                row.model_name.clone(),
            )))
        }
        "claude-code" => Ok(Resolution::DelegateToClaudeCode),
        other => anyhow::bail!("unknown provider '{other}' in model '{}'", row.id),
    }
}

/// Resolve which model to use for an interactive session.
///
/// Priority:
///   1. Explicit `default_model` in config (legacy override — phased out
///      once all callers use the model registry).
///   2. Global default row from `db::models`.
///   3. Live discovery via lmstudio/ollama if nothing is installed.
///
/// Hard-fail: if nothing is available, return an error.
pub async fn resolve_model(config: &Config, task: Option<TaskKind>) -> Result<Model> {
    match &config.default_model {
        Model::Claude(id) if !id.is_empty() => return Ok(Model::Claude(id.clone())),
        Model::LMStudio { id, url } if !id.is_empty() => {
            return Ok(Model::LMStudio {
                id: id.clone(),
                url: url.clone(),
            });
        }
        Model::Ollama { id, url } if !id.is_empty() => {
            return Ok(Model::Ollama {
                id: id.clone(),
                url: url.clone(),
            });
        }
        _ => {}
    }

    if let Ok(conn) = db::open_default()
        && let Ok(Some(row)) = db::models::default(&conn)
        && let Ok(res) = decide(&row)
        && let Resolution::Local(m) | Resolution::ServerClaude(m) = res
    {
        return Ok(m);
    }

    let available = discover_all(config).await;
    let task = task.unwrap_or(TaskKind::ToolUse);

    match select_for_task(&available, task) {
        Some(m) => Ok(to_config_model(m)),
        None => anyhow::bail!(
            "no models available — install one with `model.create ...`, or start LM Studio / Ollama with a model loaded"
        ),
    }
}

/// Estimate the context window in tokens for a model.
pub fn estimate_context_window(model: &Model) -> usize {
    use crate::discovery::classify_model;
    match model {
        Model::Claude(id) => classify_model(id, "claude").context_window,
        Model::LMStudio { id, .. } => classify_model(id, "lmstudio").context_window,
        Model::Ollama { id, .. } => classify_model(id, "ollama").context_window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(provider: &str, endpoint: Option<&str>, name: &str) -> db::models::Model {
        db::models::Model {
            id: "test".into(),
            provider: provider.into(),
            endpoint: endpoint.map(str::to_string),
            model_name: name.into(),
            is_default: true,
            enabled: true,
            created_at: String::new(),
        }
    }

    #[test]
    fn lmstudio_row_routes_local() {
        let r = decide(&row("lmstudio", Some("http://localhost:1234"), "llama3")).unwrap();
        assert!(matches!(r, Resolution::Local(Model::LMStudio { .. })));
    }

    #[test]
    fn ollama_row_routes_local() {
        let r = decide(&row("ollama", Some("http://localhost:11434"), "llama3:70b")).unwrap();
        assert!(matches!(r, Resolution::Local(Model::Ollama { .. })));
    }

    #[test]
    fn claude_code_routes_delegate() {
        let r = decide(&row("claude-code", None, "")).unwrap();
        assert!(matches!(r, Resolution::DelegateToClaudeCode));
    }

    #[test]
    fn unknown_provider_errors() {
        assert!(decide(&row("bogus", None, "")).is_err());
    }

    #[test]
    fn local_row_missing_endpoint_errors() {
        assert!(decide(&row("lmstudio", None, "llama3")).is_err());
    }
}
