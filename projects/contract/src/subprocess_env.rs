//! Subprocess environment-exposure seam.
//!
//! orca spawns child processes (MCP servers over stdio, and their children). A
//! plugin sometimes needs to expose environment to those subprocesses **without
//! core knowing the plugin exists** — e.g. the docker plugin exposes
//! `DOCKER_HOST` pointing at whichever runtime is registered and active, so a
//! docker-based MCP server talks to the right engine.
//!
//! Providers register into a process-global registry — in-process, or (for an
//! external subprocess plugin) via the [`register_from_def`] JSON proxy
//! the plugin-loader installs for `domain = "subprocess_env"`. Whoever spawns a
//! subprocess calls [`collect`] and merges the result into the child's
//! environment. Core stays domain-agnostic, exactly the way the
//! `topology` / `storage` / `web` domains already work.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One environment variable a provider exposes to spawned subprocesses.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// A source of subprocess environment. `env()` is resolved lazily at spawn time
/// so a provider can return values that depend on live state (registered docker
/// runtimes, an authenticated session, …) rather than a static snapshot.
pub trait EnvProvider: Send + Sync {
    /// Stable id — used to replace-in-place on re-register and to deregister on
    /// plugin unload.
    fn name(&self) -> &str;
    /// Environment to expose right now. Errors are non-fatal to spawning: a
    /// failing provider is skipped, never blocks the child.
    fn env(&self) -> Result<Vec<EnvVar>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn EnvProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register (or replace, by name) an env provider.
pub fn register_provider(provider: Arc<dyn EnvProvider>) {
    if let Ok(mut reg) = GLOBAL.write() {
        reg.retain(|p| p.name() != provider.name());
        reg.push(provider);
    }
}

/// Snapshot of the registered providers.
pub fn providers() -> Vec<Arc<dyn EnvProvider>> {
    GLOBAL.read().map(|r| r.clone()).unwrap_or_default()
}

/// Remove a provider by name (plugin unload). Returns whether one was present.
pub fn deregister_provider(name: &str) -> bool {
    if let Ok(mut reg) = GLOBAL.write() {
        let before = reg.len();
        reg.retain(|p| p.name() != name);
        return reg.len() != before;
    }
    false
}

/// Collect env from every registered provider, best-effort. A provider that
/// errors is logged and skipped. Later providers override earlier keys; the
/// caller (e.g. the mcp client) applies operator-supplied config *after* this,
/// so explicit per-server env always wins over a provider default.
pub fn collect() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for p in providers() {
        match p.env() {
            Ok(vars) => {
                for v in vars {
                    out.retain(|(k, _)| k != &v.key);
                    out.push((v.key, v.value));
                }
            }
            Err(e) => {
                tracing::warn!(provider = %p.name(), error = %e, "subprocess_env provider failed; skipping");
            }
        }
    }
    out
}

// ── Host-side loaded-plugin proxy ─────────────────────────────────────────

/// The synchronous invoke thunk a plugin's env provider is driven through
/// across the FFI / socket boundary (same shape the loader uses for every
/// domain backend).
type InvokeThunk = Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync>;

/// Operation name the [`EnvProviderProxy`] invokes. The plugin exposes a tool
/// `"{invoke_prefix}.{ENV_OP}"` returning a JSON `Vec<EnvVar>`.
pub const ENV_OP: &str = "env";

/// Install a plugin-backed env provider (called by the plugin-loader for
/// `domain = "subprocess_env"`).
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(EnvProviderProxy { name, invoke }));
    Ok(())
}

struct EnvProviderProxy {
    name: String,
    invoke: InvokeThunk,
}

impl EnvProvider for EnvProviderProxy {
    fn name(&self) -> &str {
        &self.name
    }
    fn env(&self) -> Result<Vec<EnvVar>> {
        let out = (self.invoke)(ENV_OP, "{}".to_string())
            .map_err(|e| anyhow::anyhow!("subprocess_env '{}' invoke failed: {e}", self.name))?;
        Ok(serde_json::from_str(&out)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(&'static str, Vec<EnvVar>);
    impl EnvProvider for Fixed {
        fn name(&self) -> &str {
            self.0
        }
        fn env(&self) -> Result<Vec<EnvVar>> {
            Ok(self.1.clone())
        }
    }
    struct Failing(&'static str);
    impl EnvProvider for Failing {
        fn name(&self) -> &str {
            self.0
        }
        fn env(&self) -> Result<Vec<EnvVar>> {
            anyhow::bail!("boom")
        }
    }

    fn ev(k: &str, v: &str) -> EnvVar {
        EnvVar {
            key: k.into(),
            value: v.into(),
        }
    }

    #[test]
    fn collect_merges_and_later_wins_and_skips_failures() {
        // Isolate: clear any providers a parallel test left behind.
        if let Ok(mut reg) = GLOBAL.write() {
            reg.clear();
        }
        register_provider(Arc::new(Fixed(
            "a",
            vec![ev("DOCKER_HOST", "unix:///a.sock")],
        )));
        register_provider(Arc::new(Failing("b")));
        register_provider(Arc::new(Fixed(
            "c",
            vec![ev("DOCKER_HOST", "tcp://c:2376")],
        )));

        let env = collect();
        // failing provider skipped; later provider overrode the key.
        assert_eq!(
            env.iter().filter(|(k, _)| k == "DOCKER_HOST").count(),
            1,
            "duplicate key not collapsed: {env:?}"
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "DOCKER_HOST")
                .map(|(_, v)| v.as_str()),
            Some("tcp://c:2376")
        );

        assert!(deregister_provider("a"));
        assert!(deregister_provider("c"));
        assert!(!deregister_provider("b_gone"));
        if let Ok(mut reg) = GLOBAL.write() {
            reg.clear();
        }
    }
}
