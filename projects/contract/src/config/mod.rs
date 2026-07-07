//! Runtime configuration for the orca binary.
//!
//! `Config::load()` reads paths and env vars only — no DB access.
//! DB startup (migrations, API key loading) is handled by `db::startup`.

mod consts;
pub mod docs;
pub mod paths;
pub use consts::*;
pub use paths::{db_path, memory_root, orca_home, pki_dir, profiles_dir, state_dir};

use anyhow::Result;
use std::path::PathBuf;

/// All runtime configuration for the orca binary.
///
/// Static config (API keys, LLM endpoints) lives here.
/// Dynamic registries (MCP servers, Docker runtimes, etc.) live in `orca.db` — see the db crate.
///
/// Field naming note: `app_dir` is the legacy name for the app-dir
/// (`~/.orca/`). The "vault" concept (`~/orca/`) is dead — see
/// `project_kill_vault.md`. The field name is kept for now to avoid a
/// repo-wide rename in this commit; treat it as `app_dir`. The standalone
/// `vault_root` field (was `~/orca/`) has been removed.
#[derive(Debug, Clone)]
pub struct Config {
    pub anthropic_api_key: Option<String>,
    pub lmstudio_url: String,
    pub ollama_url: String,
    pub default_model: Model,
    /// App state/config dir: `~/.orca/` (db, logs, memory, config, profiles).
    pub app_dir: PathBuf,
    pub memory_root: PathBuf,
    pub db_path: PathBuf,
    /// All listening ports for this daemon. Defaults from
    /// `consts::APP_REST_HTTP_PORT` / `APP_REST_HTTPS_PORT` / `APP_PLUGIN_PORT`;
    /// each is overridable via env var (`ORCA_HTTP_PORT`, `ORCA_HTTPS_PORT`,
    /// `ORCA_MESH_PORT`). Daemon code reads from here, never from the raw
    /// consts, so a single override flows through to bind, loopback URLs,
    /// pod dial targets, etc.
    pub ports: Ports,
}

/// Network port assignments for the orca daemon. All three protocols listen
/// concurrently on distinct ports; nothing collapses them.
///
/// `Ports` is a value type — resolution (DB settings + env override) lives
/// in `db::ports` because `orca-utils` cannot depend on the DB crate.
/// Server code should call `db::ports::current()` instead of
/// constructing `Ports` directly.
#[derive(Debug, Clone, Copy)]
pub struct Ports {
    /// Plain HTTP REST + UI (homelab-friendly default, no cert needed).
    pub http: u16,
    /// HTTPS REST + UI (mesh CA server cert; Caddy front later).
    pub https: u16,
    /// Pod mesh mTLS — peer-to-peer plugin RPC.
    pub mesh: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            http: consts::APP_REST_HTTP_PORT,
            https: consts::APP_REST_HTTPS_PORT,
            mesh: consts::APP_PLUGIN_PORT,
        }
    }
}

impl Ports {
    /// Layer env-var overrides onto a base set. Used by the DB-backed
    /// resolver in `db::ports` after it reads the persisted ports
    /// — env vars are the highest-precedence runtime knob and apply on
    /// top of whatever was stored.
    ///
    /// Unparseable values log a warning and pass the base through
    /// unchanged so operator typos don't take a daemon offline.
    pub fn apply_env_overrides(self) -> Self {
        Self {
            http: parse_port_env("ORCA_HTTP_PORT", self.http),
            https: parse_port_env("ORCA_HTTPS_PORT", self.https),
            mesh: parse_port_env("ORCA_MESH_PORT", self.mesh),
        }
    }
}

fn parse_port_env(name: &str, fallback: u16) -> u16 {
    match std::env::var(name) {
        Ok(raw) => raw.parse::<u16>().unwrap_or_else(|_| {
            eprintln!(
                "[orca::config] {name}={raw:?} could not be parsed as u16; using default {fallback}"
            );
            fallback
        }),
        Err(_) => fallback,
    }
}

/// Which model backend and model ID to use for a session.
///
/// Defaults to `LMStudio` (local-first). Claude is escalation-only.
/// The `url` field on LMStudio/Ollama is empty when loaded from env/config —
/// `build_backend` then falls back to the global config URL. When populated
/// from discovery it carries the specific endpoint that answered.
#[derive(Debug, Clone)]
pub enum Model {
    /// Anthropic Claude API — requires `ANTHROPIC_API_KEY` or a DB secret entry.
    Claude(String),
    /// LM Studio (OpenAI-compatible local server) — no API key needed.
    LMStudio { id: String, url: String },
    /// Ollama (OpenAI-compatible local/network server) — no API key needed.
    Ollama { id: String, url: String },
}

impl Model {
    /// Parse a /model <spec> argument.
    /// Accepts: "claude-sonnet-4-6", "claude:claude-sonnet-4-6", "lmstudio:model-id", "ollama:model-id"
    pub fn parse(s: &str) -> Self {
        if let Some(m) = s.strip_prefix("lmstudio:") {
            Model::LMStudio {
                id: m.to_string(),
                url: String::new(),
            }
        } else if let Some(m) = s.strip_prefix("ollama:") {
            Model::Ollama {
                id: m.to_string(),
                url: String::new(),
            }
        } else if let Some(m) = s.strip_prefix("claude:") {
            Model::Claude(m.to_string())
        } else if s.starts_with("claude-") {
            Model::Claude(s.to_string())
        } else {
            Model::LMStudio {
                id: s.to_string(),
                url: String::new(),
            }
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Model::Claude(id) => id,
            Model::LMStudio { id, .. } | Model::Ollama { id, .. } => id,
        }
    }
}

impl Config {
    /// Load config from the environment and filesystem paths only.
    /// No DB access — call `db::startup::init` after this to run migrations
    /// and `db::startup::load_api_key` to populate `anthropic_api_key` from
    /// the encrypted DB when no env var is set.
    pub fn load() -> Result<Self> {
        // Resolve through the canonical path module so `$ORCA_HOME` /
        // `$ORCA_DB_PATH` move the WHOLE instance (db included), not just the
        // loopback token. Previously this hard-coded `dirs::home_dir()` and
        // silently ignored `$ORCA_HOME`, so two instances shared one DB.
        let app_dir = paths::state_dir()?;
        let memory_root = paths::memory_root()?;
        let db_path = paths::db_path()?;

        let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let lmstudio_url =
            std::env::var("LMSTUDIO_URL").unwrap_or_else(|_| "http://localhost:1234".to_string());
        let ollama_url =
            std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".to_string());

        Ok(Config {
            anthropic_api_key: api_key,
            lmstudio_url,
            ollama_url,
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir,
            memory_root,
            db_path,
            // Compile-time-default ports. Runtime callers must read from
            // `db::ports::current()` to see operator overrides.
            ports: Ports::default(),
        })
    }

    pub fn orca_toml_path(&self) -> PathBuf {
        self.app_dir.join("orca.toml")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.app_dir.join("logs/sessions")
    }

    /// Deprecated: config docs are now embedded into the binary via
    /// `config::docs::get(name)` and `config::docs::list_basenames()`. Path
    /// returned here exists only for callers that haven't migrated yet — it
    /// will not exist on most installs.
    #[deprecated(note = "use config::docs::get(name) — config docs are embedded")]
    pub fn config_dir(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join("code/argyle-labs/orca/config")
    }

    /// Root directory for per-profile content: `~/.orca/profiles/`.
    /// Each profile's content lives under `<profiles_dir>/<profile-id>/`.
    pub fn profiles_dir(&self) -> PathBuf {
        self.app_dir.join(consts::APP_PROFILES_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lmstudio_prefix() {
        let m = Model::parse("lmstudio:qwen3");
        assert!(
            matches!(m, Model::LMStudio { ref id, .. } if id == "qwen3"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_claude_colon_prefix() {
        let m = Model::parse("claude:claude-opus-4-7");
        assert!(
            matches!(m, Model::Claude(ref s) if s == "claude-opus-4-7"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_claude_dash_prefix() {
        let m = Model::parse("claude-sonnet-4-6");
        assert!(
            matches!(m, Model::Claude(ref s) if s == "claude-sonnet-4-6"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_unknown_defaults_to_lmstudio() {
        let m = Model::parse("some-local-model");
        assert!(
            matches!(m, Model::LMStudio { ref id, .. } if id == "some-local-model"),
            "got: {m:?}"
        );
    }

    #[test]
    fn parse_empty_defaults_to_lmstudio() {
        let m = Model::parse("");
        assert!(
            matches!(m, Model::LMStudio { ref id, .. } if id.is_empty()),
            "got: {m:?}"
        );
    }

    // Ports resolution lives in `db::ports` because the DB is the
    // source of truth. See that module's tests for the precedence chain
    // (env > DB > const). `apply_env_overrides` is the only pure piece
    // that's testable here without a DB connection.

    #[test]
    fn apply_env_overrides_passes_through_when_unset() {
        // Don't touch env in this test — it'd race with other tests.
        // Just verify the no-op case using a sentinel base.
        let base = Ports {
            http: 11111,
            https: 22222,
            mesh: 33333,
        };
        // If env vars happen to be set in the calling shell they'd
        // override, but the typical test env has them unset. Skip the
        // assertion in that case to keep the test robust.
        let out = base.apply_env_overrides();
        if std::env::var("ORCA_HTTP_PORT").is_err()
            && std::env::var("ORCA_HTTPS_PORT").is_err()
            && std::env::var("ORCA_MESH_PORT").is_err()
        {
            assert_eq!(out.http, base.http);
            assert_eq!(out.https, base.https);
            assert_eq!(out.mesh, base.mesh);
        }
    }
}
