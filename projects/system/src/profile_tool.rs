//! `system profile <plugin>` — activate opt-in heap profiling for a plugin.
//!
//! Sets the per-plugin instrumentation flag (`contract::plugin_instrument`) so
//! the plugin's NEXT spawn injects `MALLOC_CONF=prof:true,…` +
//! `ORCA_PLUGIN_INSTRUMENT=1`. Activation follows the toggle+restart contract:
//! jemalloc reads `MALLOC_CONF` once at process start, so the verb reports that a
//! respawn is required and returns the `jeprof` recipe for collecting/diffing
//! the dumps.
//!
//! The auto-respawn + auto-collect of the dump paths is STUBBED for now (see the
//! `respawned` field): the flag is set and the manual recipe returned. A
//! follow-up wires the supervisor respawn + dump-path collection.

use contract::plugin_instrument;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfileArgs {
    /// Plugin to profile (its `target_software`, e.g. `docker`).
    #[arg(value_name = "PLUGIN")]
    pub plugin: String,
    /// Turn profiling OFF for the plugin instead of on. The plugin runs stock
    /// again after its next respawn.
    #[arg(long)]
    pub off: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProfileOutput {
    /// The plugin the flag was set for.
    pub plugin: String,
    /// Whether instrumentation is now enabled for the plugin.
    pub enabled: bool,
    /// Directory heap-profile dumps land in once the plugin respawns with
    /// profiling active. `None` if no state dir could be resolved.
    pub prof_dir: Option<String>,
    /// Whether this call respawned the plugin. Currently always `false` — the
    /// operator restarts the plugin to activate (toggle+restart contract).
    pub respawned: bool,
    /// Human-readable next step + the `jeprof` collect/diff recipe.
    pub recipe: String,
}

/// Toggle heap profiling for a plugin. Takes effect on the plugin's next spawn.
#[orca_tool(domain = "system", verb = "profile")]
async fn system_profile(
    args: ProfileArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ProfileOutput> {
    if args.plugin.trim().is_empty() {
        anyhow::bail!("profile: plugin name is required");
    }
    let on = !args.off;
    plugin_instrument::set_enabled(&args.plugin, on);

    let prof_dir = plugin_instrument::prof_dir(&args.plugin).map(|p| p.display().to_string());
    let recipe = if on {
        format!(
            "instrumentation enabled for '{plugin}'. RESTART the plugin to activate \
             (jemalloc reads MALLOC_CONF once at start).\n{jeprof}",
            plugin = args.plugin,
            jeprof =
                plugin_instrument::jeprof_recipe(&args.plugin, &format!("<{}-exe>", args.plugin)),
        )
    } else {
        format!(
            "instrumentation disabled for '{}'. RESTART the plugin to return it to stock.",
            args.plugin
        )
    };

    Ok(ProfileOutput {
        plugin: args.plugin.clone(),
        enabled: on,
        prof_dir,
        // Auto-respawn is stubbed; the operator restarts the plugin. TODO: wire
        // supervisor respawn + collect the emitted jeprof.*.heap paths here.
        respawned: false,
        recipe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        let dir = std::env::temp_dir().join(format!("orca-profile-ctx-{}", std::process::id()));
        contract::ToolCtx::new(std::sync::Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("profile-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn enables_then_disables() {
        let ctx = test_ctx();
        let out = system_profile(
            ProfileArgs {
                plugin: "t-profile-verb".into(),
                off: false,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.enabled);
        assert!(plugin_instrument::is_enabled("t-profile-verb"));
        assert!(!out.respawned);
        assert!(out.recipe.contains("RESTART"));

        let out2 = system_profile(
            ProfileArgs {
                plugin: "t-profile-verb".into(),
                off: true,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(!out2.enabled);
        assert!(!plugin_instrument::is_enabled("t-profile-verb"));
    }

    #[tokio::test]
    async fn empty_plugin_errors() {
        let ctx = test_ctx();
        let err = system_profile(ProfileArgs::default(), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("plugin name is required"));
    }
}
