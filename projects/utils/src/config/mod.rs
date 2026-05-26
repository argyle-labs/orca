//! Runtime configuration for the orca binary.
//!
//! `Config::load()` reads paths and env vars only — no DB access.
//! DB startup (migrations, API key loading) is handled by `db::startup`.

mod consts;
pub mod docs;
pub use consts::*;

use anyhow::{Context, Result};
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
    /// Resolve ports with the precedence: env var > orca.toml `[ports]` >
    /// compile-time default. Cached after the first call so hot paths
    /// (loopback URLs, mDNS, peer dial defaults) don't re-read disk per
    /// call.
    ///
    /// Operator change flow:
    ///   1. Edit `~/.orca/orca.toml` `[ports]` (persistent, per-host).
    ///   2. Or export `ORCA_HTTP_PORT=…` etc (process-scoped override).
    ///   3. Restart daemon — the cache is process-lifetime, so a restart
    ///      picks up the new values.
    pub fn from_env() -> Self {
        static CACHE: std::sync::OnceLock<Ports> = std::sync::OnceLock::new();
        *CACHE.get_or_init(Self::resolve_uncached)
    }

    /// Build the port set without consulting the cache. Public for tests
    /// that need to verify resolution from a temp HOME without process
    /// state. Production callers use `from_env`.
    pub fn resolve_uncached() -> Self {
        let toml_ports = load_ports_from_toml();
        let const_default = Self::default();
        let after_toml = Ports {
            http: toml_ports.http.unwrap_or(const_default.http),
            https: toml_ports.https.unwrap_or(const_default.https),
            mesh: toml_ports.mesh.unwrap_or(const_default.mesh),
        };
        Self {
            http: parse_port_env("ORCA_HTTP_PORT", after_toml.http),
            https: parse_port_env("ORCA_HTTPS_PORT", after_toml.https),
            mesh: parse_port_env("ORCA_MESH_PORT", after_toml.mesh),
        }
    }
}

/// Optional port overrides read from `~/.orca/orca.toml`'s `[ports]`
/// section. Each field is independent — operators can override one port
/// without specifying the others. Missing file or missing section ⇒ all
/// `None` (consts win).
#[derive(Default, serde::Deserialize)]
struct TomlPorts {
    http: Option<u16>,
    https: Option<u16>,
    mesh: Option<u16>,
}

#[derive(Default, serde::Deserialize)]
struct TomlRoot {
    #[serde(default)]
    ports: TomlPorts,
}

fn load_ports_from_toml() -> TomlPorts {
    let Some(home) = dirs::home_dir() else {
        return TomlPorts::default();
    };
    let path = home.join(consts::APP_STATE_DIR).join("orca.toml");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return TomlPorts::default();
    };
    match toml::from_str::<TomlRoot>(&raw) {
        Ok(root) => root.ports,
        Err(e) => {
            eprintln!(
                "[orca::config] {} has invalid [ports] section: {e} — falling back to defaults",
                path.display()
            );
            TomlPorts::default()
        }
    }
}

/// Convenience: current HTTP port (env-overridable, falls back to
/// `APP_REST_HTTP_PORT`). Use this in place of the const at any runtime
/// call site that wants the operator's chosen port.
pub fn http_port() -> u16 {
    Ports::from_env().http
}

/// Convenience: current HTTPS port (env-overridable).
pub fn https_port() -> u16 {
    Ports::from_env().https
}

/// Convenience: current pod-mesh mTLS port (env-overridable).
pub fn mesh_port() -> u16 {
    Ports::from_env().mesh
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
        let home = dirs::home_dir().context("no home dir")?;
        let app_dir = home.join(consts::APP_STATE_DIR);
        let memory_root = app_dir.join("memory");
        let db_path = app_dir.join(consts::APP_DB_FILE);

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
            ports: Ports::from_env(),
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
            .join("code/orca/config")
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

    // ── Ports::from_env precedence: env > orca.toml > const ────────────────
    //
    // These tests serialize on the process env vars + the HOME dir, which
    // is unsound under cargo's test-thread parallelism for siblings that
    // also read `HOME` / `ORCA_*_PORT`. We isolate via a mutex.

    use std::sync::Mutex;
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn with_isolated_env<R>(f: impl FnOnce() -> R) -> R {
        let _lock = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        for k in ["ORCA_HTTP_PORT", "ORCA_HTTPS_PORT", "ORCA_MESH_PORT"] {
            // SAFETY: tests are single-threaded under the mutex above; no
            // other thread observes the env mutation window.
            unsafe { std::env::remove_var(k) };
        }
        f()
    }

    #[test]
    fn ports_const_default_when_no_toml_no_env() {
        with_isolated_env(|| {
            let tmp = tempfile::tempdir().unwrap();
            // SAFETY: tests serialize via ENV_GUARD.
            unsafe { std::env::set_var("HOME", tmp.path()) };
            let p = Ports::resolve_uncached();
            assert_eq!(p.http, consts::APP_REST_HTTP_PORT);
            assert_eq!(p.https, consts::APP_REST_HTTPS_PORT);
            assert_eq!(p.mesh, consts::APP_PLUGIN_PORT);
        });
    }

    #[test]
    fn ports_toml_overrides_const() {
        with_isolated_env(|| {
            let tmp = tempfile::tempdir().unwrap();
            let app = tmp.path().join(consts::APP_STATE_DIR);
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(
                app.join("orca.toml"),
                "[ports]\nhttp = 18000\nhttps = 18443\nmesh = 18002\n",
            )
            .unwrap();
            unsafe { std::env::set_var("HOME", tmp.path()) };
            let p = Ports::resolve_uncached();
            assert_eq!(p.http, 18000);
            assert_eq!(p.https, 18443);
            assert_eq!(p.mesh, 18002);
        });
    }

    #[test]
    fn ports_env_overrides_toml() {
        with_isolated_env(|| {
            let tmp = tempfile::tempdir().unwrap();
            let app = tmp.path().join(consts::APP_STATE_DIR);
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(app.join("orca.toml"), "[ports]\nhttp = 18000\n").unwrap();
            unsafe {
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("ORCA_HTTP_PORT", "19000");
            }
            let p = Ports::resolve_uncached();
            assert_eq!(p.http, 19000, "env must win over toml");
        });
    }

    #[test]
    fn ports_toml_partial_overrides_only_specified_fields() {
        with_isolated_env(|| {
            let tmp = tempfile::tempdir().unwrap();
            let app = tmp.path().join(consts::APP_STATE_DIR);
            std::fs::create_dir_all(&app).unwrap();
            // Only `mesh` is specified — http + https fall back to consts.
            std::fs::write(app.join("orca.toml"), "[ports]\nmesh = 12042\n").unwrap();
            unsafe { std::env::set_var("HOME", tmp.path()) };
            let p = Ports::resolve_uncached();
            assert_eq!(p.http, consts::APP_REST_HTTP_PORT);
            assert_eq!(p.https, consts::APP_REST_HTTPS_PORT);
            assert_eq!(p.mesh, 12042);
        });
    }

    #[test]
    fn ports_malformed_toml_falls_back_to_const() {
        with_isolated_env(|| {
            let tmp = tempfile::tempdir().unwrap();
            let app = tmp.path().join(consts::APP_STATE_DIR);
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(app.join("orca.toml"), "this is not toml at all").unwrap();
            unsafe { std::env::set_var("HOME", tmp.path()) };
            let p = Ports::resolve_uncached();
            // Malformed file logs a warning but doesn't crash — consts win.
            assert_eq!(p.http, consts::APP_REST_HTTP_PORT);
        });
    }

    #[test]
    fn ports_unparseable_env_falls_back() {
        with_isolated_env(|| {
            let tmp = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("ORCA_HTTP_PORT", "not-a-number");
            }
            let p = Ports::resolve_uncached();
            assert_eq!(p.http, consts::APP_REST_HTTP_PORT);
        });
    }
}
