//! Agent backend selection.
//!
//! Determines, for a given agent invocation, whether the request runs against:
//!   - LM Studio (local model)
//!   - The Anthropic API directly (server-side, opt-in)
//!   - The calling Claude Code session (structured-return delegation)
//!
//! Configuration lives in the `settings` kv table. All failures are hard:
//! there is no silent fallback between modes.

use crate::llm::discovery::{TaskKind, discover_all, select_for_task, to_config_model};
use anyhow::{Context, Result};
use db;
use orca_utils::config::{Config, Model};

const KEY_MODE: &str = "agent_backend.mode";
const KEY_USE_SERVER_ANTHROPIC: &str = "agent_backend.use_server_anthropic";
const KEY_OVERRIDE_PREFIX: &str = "agent_backend.override.";

/// Resolution outcome — what the caller (run_agent) should do.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Run the agent locally against LM Studio. Hard-fail if unavailable — no
    /// fallback. Emitted in local mode (ignores overrides) or hybrid with a
    /// local override when the global mode is also local.
    Local(Model),
    /// Try local first; if that fails, fall through to the claude path.
    /// Emitted in claude mode when the per-agent override is "local".
    LocalThenClaude(Model),
    /// Run the agent against the Anthropic API server-side. Only emitted when
    /// `agent_backend.use_server_anthropic = true` AND a key is present in the
    /// config (env or keychain).
    ServerClaude(Model),
    /// Tell the caller (a Claude Code session) to run the agent itself via
    /// `get_agent` + `Agent(general-purpose)`. The MCP tool returns a JSON
    /// envelope rather than executing.
    DelegateToClaudeCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Local,
    Claude,
    Hybrid,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "local" => Ok(Mode::Local),
            "claude" => Ok(Mode::Claude),
            "hybrid" => Ok(Mode::Hybrid),
            other => {
                anyhow::bail!("invalid agent_backend mode '{other}' (want: local|claude|hybrid)")
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Local => "local",
            Mode::Claude => "claude",
            Mode::Hybrid => "hybrid",
        }
    }
}

/// Read the active mode from the settings table. Defaults to `local`.
pub fn current_mode() -> Result<Mode> {
    let conn = db::open_default()?;
    match db::settings::get(&conn, KEY_MODE)? {
        Some(s) => Mode::parse(&s),
        None => Ok(Mode::Local),
    }
}

pub fn set_mode(mode: Mode) -> Result<()> {
    let conn = db::open_default()?;
    db::settings::set(&conn, KEY_MODE, mode.as_str())
}

pub fn use_server_anthropic() -> Result<bool> {
    let conn = db::open_default()?;
    Ok(db::settings::get(&conn, KEY_USE_SERVER_ANTHROPIC)?
        .map(|v| v == "true")
        .unwrap_or(false))
}

pub fn set_use_server_anthropic(enabled: bool) -> Result<()> {
    let conn = db::open_default()?;
    db::settings::set(
        &conn,
        KEY_USE_SERVER_ANTHROPIC,
        if enabled { "true" } else { "false" },
    )
}

/// Per-agent override. Returns `Some("local"|"claude")` if set, else `None`.
pub fn get_override(agent: &str) -> Result<Option<String>> {
    let conn = db::open_default()?;
    db::settings::get(&conn, &format!("{KEY_OVERRIDE_PREFIX}{agent}"))
}

pub fn set_override(agent: &str, backend: &str) -> Result<()> {
    match backend {
        "local" | "claude" => {}
        other => anyhow::bail!("invalid override backend '{other}' (want: local|claude)"),
    }
    let conn = db::open_default()?;
    db::settings::set(&conn, &format!("{KEY_OVERRIDE_PREFIX}{agent}"), backend)
}

pub fn clear_override(agent: &str) -> Result<bool> {
    let conn = db::open_default()?;
    db::settings::delete(&conn, &format!("{KEY_OVERRIDE_PREFIX}{agent}"))
}

pub fn list_overrides() -> Result<Vec<(String, String)>> {
    let conn = db::open_default()?;
    let rows = db::settings::list_prefix(&conn, KEY_OVERRIDE_PREFIX)?;
    Ok(rows
        .into_iter()
        .map(|(k, v)| (k.trim_start_matches(KEY_OVERRIDE_PREFIX).to_string(), v))
        .collect())
}

/// Decide how to dispatch an agent invocation. Reads from the settings table.
pub fn resolve(agent: &str, config: &Config) -> Result<Resolution> {
    let mode = current_mode()?;
    // Overrides apply in hybrid and claude modes; in local mode the override is
    // ignored — everything is forced local with hard-fail.
    let agent_override = if matches!(mode, Mode::Hybrid | Mode::Claude) {
        get_override(agent)?
    } else {
        None
    };
    let use_server = use_server_anthropic()?;
    decide(mode, agent_override.as_deref(), use_server, config)
}

/// Pure dispatch logic — no I/O. Exposed for tests.
///
/// Decision tree:
///   local mode  → Local (hard-fail; overrides ignored)
///   claude mode → check override:
///                   "local"  → LocalThenClaude (try local, fall back to claude)
///                   "claude" → Claude path
///                   none     → Claude path
///   hybrid mode → check override:
///                   "local"  → Local (hard-fail)
///                   "claude" → Claude path
///                   none     → Claude path (default)
///
/// Claude path: ServerClaude if use_server_anthropic=true + key present,
/// else DelegateToClaudeCode.
pub fn decide(
    mode: Mode,
    agent_override: Option<&str>,
    use_server: bool,
    config: &Config,
) -> Result<Resolution> {
    let local_model = || Model::LMStudio {
        id: String::new(),
        url: String::new(),
    };

    match mode {
        Mode::Local => return Ok(Resolution::Local(local_model())),

        Mode::Claude => {
            if agent_override == Some("local") {
                return Ok(Resolution::LocalThenClaude(local_model()));
            }
            if let Some(other) = agent_override.filter(|&s| s != "claude") {
                anyhow::bail!("invalid override value: {other} (want: local|claude)");
            }
        }

        Mode::Hybrid => match agent_override {
            Some("local") => return Ok(Resolution::Local(local_model())),
            Some("claude") | None => {}
            Some(other) => anyhow::bail!("invalid override value: {other} (want: local|claude)"),
        },
    }

    if use_server {
        let model = current_claude_model(config).context(
            "agent_backend.use_server_anthropic=true but no Anthropic API key configured — \
             store one with agent_backend_set_api_key or disable the toggle",
        )?;
        return Ok(Resolution::ServerClaude(model));
    }
    Ok(Resolution::DelegateToClaudeCode)
}

/// The Claude model the user has configured. Returns None if no key is loaded
/// (since calling Anthropic without one is an error).
fn current_claude_model(config: &Config) -> Option<Model> {
    config.anthropic_api_key.as_ref()?;
    match &config.default_model {
        Model::Claude(id) if !id.is_empty() => Some(Model::Claude(id.clone())),
        // No explicit Claude model configured — leave id empty so the backend
        // layer surfaces a clear "model required" error rather than guessing.
        _ => Some(Model::Claude(String::new())),
    }
}

/// Resolve which model to use for an interactive session.
///
/// Priority order:
///   1. Explicit model in config (user already decided).
///   2. Best available model for the given task kind, discovered at call time.
///
/// Hard-fail: if nothing is available, returns an error. No silent fallback.
pub async fn resolve_model(config: &Config, task: Option<TaskKind>) -> Result<Model> {
    // Honour explicit config first.
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

    let available = discover_all(config).await;
    let task = task.unwrap_or(TaskKind::ToolUse); // default: assume tools are needed

    match select_for_task(&available, task) {
        Some(m) => Ok(to_config_model(m)),
        None => anyhow::bail!(
            "no models available — start LM Studio with a chat model loaded, \
             or run `orca login` to configure an Anthropic API key"
        ),
    }
}

/// Estimate the context window in tokens for a model. Used to warn the user
/// when context is filling up.
pub fn estimate_context_window(model: &Model) -> usize {
    use crate::llm::discovery::classify_model;
    match model {
        Model::Claude(id) => classify_model(id, "claude").context_window,
        Model::LMStudio { id, .. } => classify_model(id, "lmstudio").context_window,
        Model::Ollama { id, .. } => classify_model(id, "ollama").context_window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg_with_key(key: Option<&str>) -> Config {
        Config {
            anthropic_api_key: key.map(String::from),
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca.db"),
        }
    }

    #[test]
    fn local_mode_always_local_ignores_overrides() {
        let r = decide(Mode::Local, None, false, &cfg_with_key(None)).unwrap();
        assert!(matches!(r, Resolution::Local(_)));
        // override is ignored in local mode
        let r = decide(Mode::Local, Some("claude"), true, &cfg_with_key(Some("k"))).unwrap();
        assert!(matches!(r, Resolution::Local(_)));
    }

    #[test]
    fn claude_mode_no_override_delegates_when_server_disabled() {
        let r = decide(Mode::Claude, None, false, &cfg_with_key(Some("k"))).unwrap();
        assert!(matches!(r, Resolution::DelegateToClaudeCode));
    }

    #[test]
    fn claude_mode_no_override_server_with_key_returns_server_claude() {
        let r = decide(Mode::Claude, None, true, &cfg_with_key(Some("k"))).unwrap();
        assert!(matches!(r, Resolution::ServerClaude(_)));
    }

    #[test]
    fn claude_mode_server_without_key_errors() {
        let err = decide(Mode::Claude, None, true, &cfg_with_key(None)).unwrap_err();
        assert!(err.to_string().contains("no Anthropic API key"));
    }

    #[test]
    fn claude_mode_local_override_returns_local_then_claude() {
        let r = decide(Mode::Claude, Some("local"), false, &cfg_with_key(None)).unwrap();
        assert!(matches!(r, Resolution::LocalThenClaude(_)));
    }

    #[test]
    fn claude_mode_claude_override_routes_to_claude_path() {
        let r = decide(Mode::Claude, Some("claude"), false, &cfg_with_key(None)).unwrap();
        assert!(matches!(r, Resolution::DelegateToClaudeCode));
    }

    #[test]
    fn hybrid_no_override_defaults_to_claude_path() {
        let r = decide(Mode::Hybrid, None, false, &cfg_with_key(None)).unwrap();
        assert!(matches!(r, Resolution::DelegateToClaudeCode));
    }

    #[test]
    fn hybrid_local_override_routes_local_hard_fail() {
        let r = decide(Mode::Hybrid, Some("local"), true, &cfg_with_key(Some("k"))).unwrap();
        assert!(matches!(r, Resolution::Local(_)));
    }

    #[test]
    fn hybrid_claude_override_with_server_returns_server() {
        let r = decide(Mode::Hybrid, Some("claude"), true, &cfg_with_key(Some("k"))).unwrap();
        assert!(matches!(r, Resolution::ServerClaude(_)));
    }

    #[test]
    fn hybrid_invalid_override_errors() {
        let err = decide(Mode::Hybrid, Some("bogus"), false, &cfg_with_key(None)).unwrap_err();
        assert!(err.to_string().contains("invalid override"));
    }

    #[test]
    fn mode_parse_round_trip() {
        for s in ["local", "claude", "hybrid"] {
            assert_eq!(Mode::parse(s).unwrap().as_str(), s);
        }
        assert!(Mode::parse("offline").is_err());
    }
}
