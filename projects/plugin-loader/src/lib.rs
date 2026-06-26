// The tool surface crosses this loader as opaque JSON (`sj::Value`) — the JSON
// dispatch protocol of the type-erased boundary, identical to dispatch's
// `ErasedTool::run_json`. This is the designated opaque seam; the workspace
// disallowed-types lint is suppressed for this file only.
#![allow(clippy::disallowed_types)]

//! Runtime loader for ABI-stable cdylib plugins.
//!
//! ## What this crate does
//!
//! 1. `dlopen`s a cdylib plugin via [`abi_stable`]'s [`RootModule::load_from_file`],
//!    which runs the layout+version check and returns a `PluginModRef` — or a
//!    clean `LibraryError` if the plugin's ABI is incompatible. No UB path.
//! 2. Reads the plugin's version header (`PluginMod` metadata accessors) and
//!    verifies its declared `orca_compat` range admits the running orca version.
//! 3. Calls `PluginMod::manifest` to learn the plugin's tool surface and
//!    registers each tool into a process-global runtime registry.
//! 4. Exposes [`dispatch`] — the same `(name, args, ctx) -> Result<Value>` shape
//!    as `dispatch::dispatch` — which tries the runtime plugin registry first
//!    and falls back to the statically-linked inventory registry.
//!
//! ## Why a parallel registry
//!
//! orca's built-in tool registry is a frozen `OnceLock<ToolCache>` populated
//! once from `inventory::iter` (link-time). It has no runtime insertion path,
//! by design. Dynamically-loaded plugins therefore live in *this* registry,
//! and [`dispatch`] fronts both so callers see one tool namespace.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::RwLock;

use abi_stable::library::{LibraryError, RootModule};
use abi_stable::std_types::{RResult, RStr};
use anyhow::{Context, Result, anyhow, bail};
use contract::ToolCtx;
use plugin_toolkit::abi::{PluginModRef, ToolDef};
// `Value` is the JSON dispatch protocol across the type-erased tool boundary —
// the same opaque layer `dispatch::ErasedTool::run_json` uses. Aliased so the
// payload type is named once, here, at the designated opaque seam.
use serde_json as sj;

/// A single dynamically-loaded plugin, kept alive for the process lifetime.
///
/// The `PluginModRef` borrows from the `'static` library image abi_stable keeps
/// mapped after load, so it is safe to store and call indefinitely.
struct LoadedPlugin {
    /// `target_software` reported by the plugin header, e.g. `"jellyfin"`.
    software: String,
    /// The plugin's own semver.
    semver: String,
    /// Free-form target-software compat range, e.g. `"10.8-10.10"`.
    target_compat: String,
    /// The orca-version semver range the plugin declared.
    orca_compat: String,
    /// The ABI root module — `manifest()` / `invoke()` entrypoints.
    module: PluginModRef,
    /// Tool defs parsed from `manifest()` at load time, keyed by tool name.
    tools: HashMap<String, ToolDef>,
}

/// Process-global registry of loaded plugins, keyed by tool name → plugin index.
struct Registry {
    plugins: Vec<LoadedPlugin>,
    by_tool: HashMap<String, usize>,
}

static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();

fn registry() -> &'static RwLock<Registry> {
    REGISTRY.get_or_init(|| {
        RwLock::new(Registry {
            plugins: Vec::new(),
            by_tool: HashMap::new(),
        })
    })
}

/// Outcome of a successful load — what got registered, for the caller to log.
#[derive(Debug, Clone)]
pub struct LoadReport {
    /// `target_software` from the plugin header.
    pub software: String,
    /// The plugin's own semver.
    pub semver: String,
    /// Names of the tools registered from this plugin.
    pub tools: Vec<String>,
}

/// Load a cdylib plugin from `path`, run the full compatibility gate, and
/// register its tool surface into the runtime registry.
///
/// `orca_version` is the running orca version (e.g. from `ORCA_VERSION`); it is
/// checked against the plugin's declared `orca_compat` semver range. Returns a
/// [`LoadReport`] on success, or an error describing exactly which gate failed.
pub fn load_plugin(path: &Path, orca_version: &str) -> Result<LoadReport> {
    // ── Gate 1: abi_stable layout + version check (clean refusal, never UB) ──
    let module: PluginModRef = PluginModRef::load_from_file(path)
        .map_err(|e: LibraryError| anyhow!("ABI/layout check failed for {path:?}: {e}"))?;

    // ── Read the version header ──────────────────────────────────────────────
    let software = module.target_software()().to_string();
    let semver = module.plugin_semver()().to_string();
    let target_compat = module.target_compat()().to_string();
    let orca_compat = module.orca_compat()().to_string();

    // ── Gate 2: semantic orca-version compatibility ──────────────────────────
    let req = semver::VersionReq::parse(&orca_compat).with_context(|| {
        format!("plugin '{software}' has unparseable orca_compat '{orca_compat}'")
    })?;
    let running = semver::Version::parse(strip_pre_build(orca_version))
        .with_context(|| format!("unparseable running orca version '{orca_version}'"))?;
    if !req.matches(&running) {
        bail!(
            "plugin '{software}' v{semver} requires orca {orca_compat}, but running orca is {orca_version}"
        );
    }

    // ── Parse the tool manifest ──────────────────────────────────────────────
    let manifest_json = module.manifest()().to_string();
    let defs: Vec<ToolDef> = sj::from_str(&manifest_json)
        .with_context(|| format!("plugin '{software}' returned an invalid manifest"))?;
    let tools: HashMap<String, ToolDef> = defs.into_iter().map(|d| (d.name.clone(), d)).collect();
    let mut tool_names: Vec<String> = tools.keys().cloned().collect();
    tool_names.sort();

    // ── Register, refusing names already known (built-in or another plugin) ──
    let mut reg = registry().write().expect("plugin registry poisoned");
    for name in &tool_names {
        if reg.by_tool.contains_key(name) {
            bail!("plugin '{software}' tool '{name}' collides with an already-loaded plugin tool");
        }
        if dispatch::tool_exists(name) {
            bail!("plugin '{software}' tool '{name}' collides with a built-in tool");
        }
    }
    let idx = reg.plugins.len();
    for name in &tool_names {
        reg.by_tool.insert(name.clone(), idx);
    }
    reg.plugins.push(LoadedPlugin {
        software: software.clone(),
        semver: semver.clone(),
        target_compat: target_compat.clone(),
        orca_compat: orca_compat.clone(),
        module,
        tools,
    });

    tracing::info!(
        plugin = %software,
        version = %semver,
        target_compat = %target_compat,
        tools = ?tool_names,
        "loaded cdylib plugin"
    );

    Ok(LoadReport {
        software,
        semver,
        tools: tool_names,
    })
}

/// The plugin tool manifest entries for every loaded plugin, in load order.
/// Lets the host merge dynamic tools into MCP/OpenAPI surfaces.
pub fn loaded_tool_defs() -> Vec<ToolDef> {
    let reg = registry().read().expect("plugin registry poisoned");
    reg.plugins
        .iter()
        .flat_map(|p| p.tools.values().cloned())
        .collect()
}

/// Header + tool-name summary of one loaded plugin. The plugin-management tool
/// surface (`plugin.list`) reads this to report what is live in-process,
/// distinct from what is merely present on disk or known in the catalog.
#[derive(Debug, Clone)]
pub struct LoadedPluginInfo {
    /// `target_software` from the header, e.g. `"jellyfin"`.
    pub software: String,
    /// The plugin's own semver.
    pub semver: String,
    /// Free-form target-software compat range.
    pub target_compat: String,
    /// The orca-version semver range the plugin declared.
    pub orca_compat: String,
    /// Sorted names of the tools this plugin registered.
    pub tools: Vec<String>,
}

/// Summaries of every currently-loaded plugin, in load order. Drives
/// `plugin.list`'s "loaded" column.
pub fn loaded_plugins() -> Vec<LoadedPluginInfo> {
    let reg = registry().read().expect("plugin registry poisoned");
    reg.plugins
        .iter()
        .map(|p| {
            let mut tools: Vec<String> = p.tools.keys().cloned().collect();
            tools.sort();
            LoadedPluginInfo {
                software: p.software.clone(),
                semver: p.semver.clone(),
                target_compat: p.target_compat.clone(),
                orca_compat: p.orca_compat.clone(),
                tools,
            }
        })
        .collect()
}

/// Whether a plugin reporting `software` as its `target_software` is currently
/// loaded in the runtime registry.
pub fn is_loaded(software: &str) -> bool {
    let reg = registry().read().expect("plugin registry poisoned");
    reg.plugins.iter().any(|p| p.software == software)
}

/// Unregister every loaded plugin whose `target_software` matches `software`,
/// dropping its tool-name routes so the names free up again.
///
/// Note: this removes the plugin from the *routing* registry; abi_stable keeps
/// the underlying library image mapped for the process lifetime (there is no
/// safe unmap once a `PluginModRef` has been handed out). A reinstall under the
/// same name therefore re-registers cleanly, and the orphaned image is reclaimed
/// at process exit. Returns the number of plugins removed.
pub fn unload_plugin(software: &str) -> usize {
    let mut reg = registry().write().expect("plugin registry poisoned");
    let before = reg.plugins.len();
    let removed_tools: Vec<String> = reg
        .plugins
        .iter()
        .filter(|p| p.software == software)
        .flat_map(|p| p.tools.keys().cloned())
        .collect();
    reg.plugins.retain(|p| p.software != software);
    for name in &removed_tools {
        reg.by_tool.remove(name);
    }
    // Tool→index map points into `plugins` by position; rebuild it after a
    // retain shifts indices.
    reg.by_tool.clear();
    let rebuilt: Vec<(String, usize)> = reg
        .plugins
        .iter()
        .enumerate()
        .flat_map(|(idx, p)| p.tools.keys().cloned().map(move |n| (n, idx)))
        .collect();
    for (name, idx) in rebuilt {
        reg.by_tool.insert(name, idx);
    }
    before - reg.plugins.len()
}

/// Dispatch a tool call. Tries the dynamically-loaded plugin registry first;
/// on a miss, falls back to the statically-linked `dispatch::dispatch`. This is
/// the entrypoint the host's MCP/REST/CLI paths should call instead of
/// `dispatch::dispatch` directly, so loaded plugins share one tool namespace.
pub async fn dispatch(name: &str, args: sj::Value, ctx: &ToolCtx) -> Result<sj::Value> {
    if let Some(result) = invoke_plugin(name, &args) {
        return result;
    }
    dispatch::dispatch(name, args, ctx).await
}

/// Look the tool up in the plugin registry and, if found, marshal
/// args → JSON → `invoke()` → JSON → result. Returns `None` when no loaded
/// plugin owns `name`, so the caller can fall through to the built-in registry.
///
/// `invoke()` is synchronous across the FFI boundary; the plugin drives its own
/// async runtime internally. Exposed (not just used by [`dispatch`]) so a host
/// can route a known-plugin tool without the fallback hop.
pub fn invoke_plugin(name: &str, args: &sj::Value) -> Option<Result<sj::Value>> {
    let reg = registry().read().expect("plugin registry poisoned");
    let idx = *reg.by_tool.get(name)?;
    let plugin = &reg.plugins[idx];
    let args_json = match sj::to_string(args) {
        Ok(s) => s,
        Err(e) => return Some(Err(anyhow!("failed to encode args for '{name}': {e}"))),
    };
    let result = (plugin.module.invoke())(RStr::from_str(name), RStr::from_str(&args_json));
    Some(match result {
        RResult::ROk(out) => sj::from_str(out.as_str()).with_context(|| {
            format!(
                "plugin '{}' returned invalid JSON for '{name}'",
                plugin.software
            )
        }),
        RResult::RErr(msg) => Err(anyhow!("plugin tool '{name}' failed: {msg}")),
    })
}

/// Strip a `-pre` / `+build` suffix so a `-dev`-tagged orca build still parses
/// as a clean semver for range matching (we match on the release triple).
fn strip_pre_build(v: &str) -> &str {
    let v = v.strip_prefix('v').unwrap_or(v);
    let end = v.find(['-', '+']).unwrap_or(v.len());
    &v[..end]
}
