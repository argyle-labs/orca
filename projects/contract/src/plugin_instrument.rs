//! Parent side of the opt-in plugin instrumentation framework.
//!
//! The daemon activates heap profiling + auto-diagnostics for a *specific*
//! plugin by respawning it with instrumentation env. This module is the single
//! propagation point: it holds the per-plugin enable set and builds the env the
//! supervisor injects when spawning a plugin subprocess.
//!
//! **Opt-in invariant.** A plugin not in the enable set gets NO instrumentation
//! env — [`env_for`] returns empty — so it runs stock (the toolkit's jemalloc
//! substrate stays inert without `MALLOC_CONF`). The plugin side reads
//! `plugin_toolkit::instrument` counterparts.
//!
//! **Toggle+restart contract.** jemalloc reads `MALLOC_CONF` once, at process
//! start. Flipping a plugin on (via [`set_enabled`]) therefore only takes effect
//! on the plugin's *next spawn* — the `system profile` verb requests that
//! respawn.

use std::collections::HashSet;
use std::sync::{LazyLock, RwLock};

/// The plugin-side env marker (mirrors `plugin_toolkit::instrument::INSTRUMENT_ENV`).
pub const INSTRUMENT_ENV: &str = "ORCA_PLUGIN_INSTRUMENT";

/// jemalloc runtime config key. Activating profiling means setting this before
/// the plugin process starts.
pub const MALLOC_CONF_ENV: &str = "MALLOC_CONF";

/// Heap-profile sampling granularity: sample one allocation per `2^19` bytes
/// (~512 KiB) — low overhead, enough resolution to localize a leak.
const LG_PROF_SAMPLE: u32 = 19;

/// Per-plugin enable set. Small (a handful of plugins profiled at a time), so a
/// plain `RwLock<HashSet>` is ample.
static ENABLED: LazyLock<RwLock<HashSet<String>>> = LazyLock::new(|| RwLock::new(HashSet::new()));

/// Enable or disable instrumentation for `plugin`. Takes effect on the plugin's
/// next spawn (see the toggle+restart contract). Returns the previous state.
pub fn set_enabled(plugin: &str, on: bool) -> bool {
    let mut g = ENABLED.write().expect("instrument enable set poisoned");
    let was = g.contains(plugin);
    if on {
        g.insert(plugin.to_string());
    } else {
        g.remove(plugin);
    }
    was
}

/// Whether instrumentation is enabled for `plugin`.
pub fn is_enabled(plugin: &str) -> bool {
    ENABLED
        .read()
        .expect("instrument enable set poisoned")
        .contains(plugin)
}

/// Snapshot of every plugin with instrumentation enabled.
pub fn enabled_plugins() -> Vec<String> {
    let mut v: Vec<String> = ENABLED
        .read()
        .expect("instrument enable set poisoned")
        .iter()
        .cloned()
        .collect();
    v.sort();
    v
}

/// The directory heap-profile dumps land in for `plugin`:
/// `<state_dir>/logs/jeprof/<plugin>/`. Best-effort created. `None` if no state
/// dir can be resolved.
pub fn prof_dir(plugin: &str) -> Option<std::path::PathBuf> {
    let dir = crate::config::state_dir()
        .ok()?
        .join(crate::config::APP_LOGS_SUBDIR)
        .join("jeprof")
        .join(plugin);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::debug!(?dir, error = %e, "could not create plugin prof dir");
    }
    Some(dir)
}

/// Build the instrumentation env for a plugin about to be spawned. Empty when
/// the plugin is not enabled (opt-in). When enabled, returns:
///
/// - `ORCA_PLUGIN_INSTRUMENT=1` — the plugin-side gate for its auto-diagnostics
///   provider;
/// - `MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:<dir>/jeprof`
///   — activates jemalloc heap profiling, dumping to the plugin's prof dir.
///
/// The supervisor merges these into the child's environment at spawn.
pub fn env_for(plugin: &str) -> Vec<(String, String)> {
    if !is_enabled(plugin) {
        return Vec::new();
    }
    let mut out = vec![(INSTRUMENT_ENV.to_string(), "1".to_string())];
    let mut malloc_conf = format!("prof:true,prof_active:true,lg_prof_sample:{LG_PROF_SAMPLE}");
    if let Some(dir) = prof_dir(plugin) {
        malloc_conf.push_str(&format!(",prof_prefix:{}/jeprof", dir.display()));
    }
    out.push((MALLOC_CONF_ENV.to_string(), malloc_conf));
    out
}

/// The `jeprof` recipe an operator runs to inspect / diff the dumps a profiled
/// plugin produced. `exe` is the plugin binary path (for symbolization).
pub fn jeprof_recipe(plugin: &str, exe: &str) -> String {
    let dir = prof_dir(plugin)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| format!("<state_dir>/logs/jeprof/{plugin}"));
    format!(
        "Heap dumps for '{plugin}' land in {dir}/ as jeprof.*.heap.\n\
         Inspect the latest:   jeprof --text {exe} {dir}/jeprof.<pid>.<seq>.heap\n\
         Diff two dumps:       jeprof --base {dir}/jeprof.<early>.heap {exe} {dir}/jeprof.<late>.heap\n\
         (a growing delta between two dumps localizes the leak by call site.)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_plugin_gets_empty_env() {
        set_enabled("t-disabled", false);
        assert!(env_for("t-disabled").is_empty());
        assert!(!is_enabled("t-disabled"));
    }

    #[test]
    fn enabled_plugin_gets_instrument_and_malloc_conf() {
        set_enabled("t-enabled", true);
        assert!(is_enabled("t-enabled"));
        let env = env_for("t-enabled");
        let marker = env.iter().find(|(k, _)| k == INSTRUMENT_ENV);
        assert_eq!(marker.map(|(_, v)| v.as_str()), Some("1"));
        let mc = env
            .iter()
            .find(|(k, _)| k == MALLOC_CONF_ENV)
            .expect("MALLOC_CONF present");
        assert!(mc.1.contains("prof:true"), "{}", mc.1);
        assert!(mc.1.contains("lg_prof_sample:19"), "{}", mc.1);
        // Cleanup so a parallel test isn't affected.
        set_enabled("t-enabled", false);
    }

    #[test]
    fn set_enabled_reports_previous_and_toggles() {
        set_enabled("t-toggle", false);
        assert!(!set_enabled("t-toggle", true), "was disabled");
        assert!(set_enabled("t-toggle", false), "was enabled");
        assert!(!is_enabled("t-toggle"));
    }

    #[test]
    fn jeprof_recipe_names_binary_and_dir() {
        let r = jeprof_recipe("demo", "/opt/plugins/demo");
        assert!(r.contains("/opt/plugins/demo"));
        assert!(r.contains("--base"));
    }
}
