//! Core plugin-management tool surface (`plugin.*`).
//!
//! The plugin *install surface* on top of the out-of-process (subprocess)
//! loader (`plugin-loader`). It gives operators three things, routed through the
//! `plugin_toolkit::prelude` gateway like any other tool:
//!
//! * `plugin.list` — the embedded first-party catalog joined with whatever is
//!   installed on disk and whatever is loaded live.
//! * `plugin.install` — **sideload** an executable plugin from a local file. The
//!   plugin is spawned and completes the `plugin-proto` wire handshake *before*
//!   anything is copied; only a plugin that handshakes cleanly lands in the
//!   install dir and registers live. A catalog-name install (auto-download) is
//!   also supported.
//! * `plugin.uninstall` — remove a plugin from the install dir and unregister
//!   its tools.
//!
//! ## Why this lives in `system/`
//!
//! `system` already owns the install/update tool surface. It already depends on
//! `dispatch` and the plugin crates, so adding `plugin-loader` introduces no
//! cycle (`plugin-loader` depends only on `dispatch`/`contract`/`plugin-toolkit`,
//! none of which depend on `system`). A standalone "plugin-manager" crate would
//! be a re-export hub over the loader for no gain; the tools belong next to the
//! other core lifecycle tools.
//!
//! ## Install dir
//!
//! `orca_home()/plugins/` (reusing `files::ops::orca_home` — `$ORCA_HOME` or
//! `$HOME/.orca`). Each plugin executable is stored under a deterministic name
//! derived from its `target_software` so a reinstall overwrites cleanly and the
//! startup scan can spawn every executable it finds.

// `plugin.invoke` is a generic seam that forwards a free-form argument map to,
// and returns a free-form value from, any loaded plugin verb — genuinely opaque
// JSON at this boundary, so `serde_json::Value` is intentional here.
#![allow(clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use contract::RemoteExec;
use plugin_toolkit::prelude::{Context, JsonSchema, Result, ToolCtx, bail, orca_tool};
use plugin_toolkit::serde_json;
use serde::{Deserialize, Serialize};

/// The running orca version, baked in by `system`'s `build.rs`. The loader
/// checks this against each plugin's declared `orca_compat` range.
const ORCA_VERSION: &str = env!("ORCA_VERSION");

/// Embedded first-party catalog. Adding a plugin = adding a JSON entry.
const CATALOG_JSON: &str = include_str!("plugin_catalog.json");

// ── Catalog ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CatalogFile {
    plugins: Vec<CatalogEntry>,
}

/// One first-party plugin known to orca. `status` is `"available"` when the
/// external repo publishes a per-target release asset and the plugin is
/// installable via `plugin.install --name` today; `"unreleased"` when the repo
/// exists and is actively developed but has cut no release yet (install-by-name
/// is refused; `--file` still sideloads); `"planned"` for a first-party plugin
/// not yet extracted to its own repo.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    /// Catalog name, e.g. `"jellyfin"`. Matches the plugin's `target_software`.
    pub name: String,
    /// External software the plugin integrates, e.g. `"jellyfin"`.
    pub target_software: String,
    /// Public GitHub repo hosting the plugin's source + release pipeline.
    pub repo_url: String,
    /// Where to read about the plugin.
    pub docs_url: String,
    /// `"available"` (installable via `--name`), `"unreleased"` (repo exists,
    /// no release asset yet), or `"planned"` (not yet extracted to its own repo).
    pub status: String,
}

/// Parse the embedded catalog. Invalid embedded JSON is a build-time bug, so we
/// surface it as an error rather than panicking in a tool body.
fn catalog() -> Result<Vec<CatalogEntry>> {
    let parsed: CatalogFile =
        serde_json::from_str(CATALOG_JSON).context("embedded plugin catalog is not valid JSON")?;
    Ok(parsed.plugins)
}

/// Canonical GitHub-hosted catalog manifest — the SAME file on `main`. A
/// successful runtime refresh supersedes the embedded copy, so adding or
/// updating a plugin entry needs only a merge to `main`, not a new orca release.
const REMOTE_CATALOG_URL: &str = "https://raw.githubusercontent.com/argyle-labs/orca/main/projects/system/src/plugin_catalog.json";

/// In-process TTL for the refreshed catalog, so `plugin.list`/`plugin.install`
/// don't hit GitHub on every call.
const CATALOG_TTL: std::time::Duration = std::time::Duration::from_secs(600);

type CatalogCache = std::sync::Mutex<Option<(std::time::Instant, Vec<CatalogEntry>)>>;

fn catalog_cache() -> &'static CatalogCache {
    static CACHE: std::sync::OnceLock<CatalogCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// The catalog to use: the embedded default, overlaid by a runtime refresh from
/// [`REMOTE_CATALOG_URL`] when reachable (cached for [`CATALOG_TTL`]). Any
/// failure — offline, parse error, empty — silently falls back to the embedded
/// catalog, so installs still work air-gapped. This is the hybrid model: ship a
/// default in the binary, prefer the live manifest when we can reach it.
async fn catalog_resolved() -> Vec<CatalogEntry> {
    if let Some((at, cached)) = catalog_cache().lock().unwrap().as_ref()
        && at.elapsed() < CATALOG_TTL
    {
        return cached.clone();
    }
    let resolved = match fetch_remote_catalog().await {
        Ok(entries) if !entries.is_empty() => {
            tracing::debug!(
                count = entries.len(),
                "refreshed plugin catalog from remote manifest"
            );
            entries
        }
        Ok(_) => catalog().unwrap_or_default(),
        Err(e) => {
            tracing::debug!(
                error = %format!("{e:#}"),
                "remote plugin-catalog refresh failed; using embedded catalog"
            );
            catalog().unwrap_or_default()
        }
    };
    *catalog_cache().lock().unwrap() = Some((std::time::Instant::now(), resolved.clone()));
    resolved
}

/// Fetch + parse the remote catalog manifest. Short timeout — this is a
/// best-effort overlay, never a hard dependency.
async fn fetch_remote_catalog() -> Result<Vec<CatalogEntry>> {
    let client = utils::http::Client::new();
    let parsed: CatalogFile = client
        .get(REMOTE_CATALOG_URL.to_string())
        .header("User-Agent", format!("orca/{ORCA_VERSION}"))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .context("fetch remote plugin catalog")?
        .json()
        .context("remote plugin catalog is not valid JSON")?;
    Ok(parsed.plugins)
}

// ── Install dir ──────────────────────────────────────────────────────────────

/// Absolute path to the plugin install dir, `orca_home()/plugins/`. `None` only
/// in sealed sandboxes where neither `$ORCA_HOME` nor `$HOME` is set.
pub fn install_dir() -> Option<PathBuf> {
    files::ops::orca_home().map(|h| h.join("plugins"))
}

/// Install-dir filename for a plugin keyed by its `target_software`: the bare
/// executable name (e.g. `jellyfin`). Deterministic so a reinstall overwrites
/// and the startup scan can spawn every executable it finds.
fn install_filename(software: &str) -> String {
    software.to_string()
}

/// Set the owner-executable bit on a freshly-written plugin file so the startup
/// scan (and `spawn_plugin`) can exec it. No-op on non-unix.
#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

// ── Startup scan ──────────────────────────────────────────────────────────────

/// Scan the install dir and spawn every executable plugin found. Called once on
/// daemon startup. Each plugin is handshaked independently; a failed one is
/// logged and skipped — never fatal, so one bad sideload can't keep the daemon
/// down. Returns `(loaded, failed)` software-name lists for the caller to log.
pub fn scan_and_load() -> (Vec<String>, Vec<String>) {
    let Some(dir) = install_dir() else {
        tracing::debug!("no orca_home; skipping plugin install-dir scan");
        return (Vec::new(), Vec::new());
    };
    if !dir.exists() {
        return (Vec::new(), Vec::new());
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot read plugin install dir");
            return (Vec::new(), Vec::new());
        }
    };
    let mut loaded = Vec::new();
    let mut failed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // On non-unix there is no subprocess loader; nothing in the dir is
        // spawnable, so the bindings below are intentionally unused there.
        #[cfg(not(unix))]
        let _ = (&path, fname);
        // Plugins are standalone executables in the install dir, spawned as
        // capability-delegated subprocesses. The subprocess path is unix-only
        // (UDS wire protocol); on other platforms these files are skipped.
        #[cfg(unix)]
        if is_executable_plugin(&path) {
            // The install-dir filename is the authoritative plugin id; validate
            // the plugin's handshake against it and use it as the principal.
            match plugin_loader::spawn_plugin(&path, Some(fname)) {
                Ok(report) => {
                    apply_plugin_schema(&report);
                    tracing::info!(
                        plugin = %report.software,
                        version = %report.semver,
                        tools = ?report.tools,
                        "spawned out-of-process plugin on startup"
                    );
                    loaded.push(report.software);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %format!("{e:#}"),
                        "skipping failed subprocess plugin on startup"
                    );
                    failed.push(fname.to_string());
                }
            }
        }
    }
    (loaded, failed)
}

/// A regular, executable file in the install dir — the shape of an
/// out-of-process plugin binary. Non-executable files (READMEs, icons, stray
/// configs) and directories are ignored so the scan stays tolerant of unrelated
/// contents.
#[cfg(unix)]
fn is_executable_plugin(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
}

/// Apply a freshly-loaded plugin's declared SQL schemas into its isolated
/// namespace. The plugin declared the shapes; orca owns the db and performs the
/// migration. Best-effort + logged: a schema failure is surfaced loudly but does
/// not unload an already-registered plugin (its tools/backends still work; the
/// operator sees the migration error and can fix the declaration). A plugin that
/// declares nothing is a clean no-op.
fn apply_plugin_schema(report: &plugin_loader::LoadReport) {
    if report.declared_schema.tables.is_empty() {
        return;
    }
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(plugin = %report.software, error = %format!("{e:#}"),
                "could not open db to apply plugin schema");
            return;
        }
    };
    match db::plugin_tables::apply_decl(&conn, &report.declared_schema) {
        Ok(reports) => tracing::info!(
            plugin = %report.software,
            namespace = %report.declared_schema.namespace,
            tables = reports.len(),
            "applied plugin-declared SQL schema"
        ),
        Err(e) => tracing::warn!(
            plugin = %report.software,
            error = %format!("{e:#}"),
            "plugin schema migration failed"
        ),
    }
}

// ── plugin.list ────────────────────────────────────────────────────────────

/// Per-plugin load status reported by `plugin.list`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum PluginLoadStatus {
    /// Present on disk and currently loaded in-process.
    Loaded,
    /// In the catalog but neither installed nor loaded.
    NotInstalled,
    /// Installed on disk but not loaded — usually a failed compat gate, or
    /// installed after startup with no live registration yet.
    InstalledNotLoaded,
}

/// Full per-plugin record returned by `plugin.detail` — the heavy shape, incl.
/// the plugin's full `tools` list and compat ranges. `plugin.list` returns the
/// thin [`PluginListRow`] instead; call `plugin.detail <name>` for this.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginDetailOutput {
    /// Plugin / target-software name.
    pub name: String,
    /// Catalog metadata, when this name is a known first-party plugin. Sideloaded
    /// third-party plugins not in the catalog have `None`.
    pub catalog: Option<CatalogEntry>,
    /// Loaded semver, when live in-process.
    pub installed_version: Option<String>,
    /// Target-software compat range, when loaded.
    pub target_compat: Option<String>,
    /// orca-version compat range the loaded plugin declared.
    pub orca_compat: Option<String>,
    /// Tool names this plugin contributes, when loaded. (Heavy — detail only.)
    pub tools: Vec<String>,
    /// Whether the plugin is a known first-party catalog entry, sideloaded, or
    /// merely planned.
    pub status: PluginLoadStatus,
    /// True when this plugin is not in the catalog (a sideloaded third party).
    pub sideloaded: bool,
}

/// One thin row in `plugin.list`: identity + status + a tool COUNT (never the
/// full tool array — that lives on `plugin.detail`, keeping list thin + fast).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginListRow {
    /// Plugin / target-software name.
    pub name: String,
    /// Catalog metadata, when this name is a known first-party plugin.
    pub catalog: Option<CatalogEntry>,
    /// Loaded semver, when live in-process.
    pub installed_version: Option<String>,
    /// Number of tools this plugin contributes when loaded (0 otherwise). The
    /// tool NAMES are on `plugin.detail`.
    pub tool_count: usize,
    /// Whether the plugin is a known first-party catalog entry, sideloaded, or
    /// merely planned.
    pub status: PluginLoadStatus,
    /// True when this plugin is not in the catalog (a sideloaded third party).
    pub sideloaded: bool,
}

impl PluginListRow {
    /// Project a heavy [`PluginDetailOutput`] down to the thin list row.
    fn from_detail(d: &PluginDetailOutput) -> Self {
        PluginListRow {
            name: d.name.clone(),
            catalog: d.catalog.clone(),
            installed_version: d.installed_version.clone(),
            tool_count: d.tools.len(),
            status: d.status.clone(),
            sideloaded: d.sideloaded,
        }
    }
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginListArgs {
    /// Max plugins to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginListOutput {
    /// Thin rows for this page (catalog joined with installed + loaded state).
    pub plugins: Vec<PluginListRow>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total plugins across all pages (catalog is small + fully known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// List the first-party catalog joined with installed + loaded plugins — THIN +
/// paginated. Each row carries a `tool_count`; call `plugin.detail <name>` for a
/// plugin's full tool list and compat ranges.
#[orca_tool(domain = "plugin", verb = "list")]
async fn plugin_list(args: PluginListArgs, _ctx: &ToolCtx) -> Result<PluginListOutput> {
    let catalog = catalog_resolved().await;
    let loaded = plugin_loader::loaded_plugins();
    let installed_on_disk = installed_software_on_disk();
    let details = build_plugin_list_rows(&catalog, &loaded, &installed_on_disk);
    let rows: Vec<PluginListRow> = details.iter().map(PluginListRow::from_detail).collect();
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(rows, &params);
    Ok(PluginListOutput {
        plugins: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDetailArgs {
    /// Plugin / target-software name to inspect.
    pub name: String,
}

/// Full detail for ONE plugin — the heavy shape (full tool list + compat ranges).
/// The fan-out companion to the thin `plugin.list`.
#[orca_tool(domain = "plugin", verb = "detail")]
async fn plugin_detail(args: PluginDetailArgs, _ctx: &ToolCtx) -> Result<PluginDetailOutput> {
    let catalog = catalog_resolved().await;
    let loaded = plugin_loader::loaded_plugins();
    let installed_on_disk = installed_software_on_disk();
    let details = build_plugin_list_rows(&catalog, &loaded, &installed_on_disk);
    details
        .into_iter()
        .find(|d| d.name == args.name)
        .with_context(|| format!("no such plugin: {}", args.name))
}

/// Pure join/dedup behind `plugin.list`: catalog rows first (in catalog order,
/// joined to the live/on-disk state), then any loaded/installed plugin not in
/// the catalog as a sorted, deduped sideloaded tail. Split out from the tool
/// body so the row-building logic is testable without the live registry / disk.
fn build_plugin_list_rows(
    catalog: &[CatalogEntry],
    loaded: &[plugin_loader::LoadedPluginInfo],
    installed_on_disk: &[String],
) -> Vec<PluginDetailOutput> {
    let mut rows: Vec<PluginDetailOutput> = Vec::new();

    // Catalog rows first, in catalog order.
    for entry in catalog {
        let live = loaded.iter().find(|l| l.software == entry.target_software);
        let on_disk = installed_on_disk.contains(&entry.target_software);
        let status = match (live.is_some(), on_disk) {
            (true, _) => PluginLoadStatus::Loaded,
            (false, true) => PluginLoadStatus::InstalledNotLoaded,
            (false, false) => PluginLoadStatus::NotInstalled,
        };
        rows.push(PluginDetailOutput {
            name: entry.name.clone(),
            catalog: Some(entry.clone()),
            installed_version: live.map(|l| l.semver.clone()),
            target_compat: live.map(|l| l.target_compat.clone()),
            orca_compat: live.map(|l| l.orca_compat.clone()),
            tools: live.map(|l| l.tools.clone()).unwrap_or_default(),
            status,
            sideloaded: false,
        });
    }

    // Then any loaded/installed plugin NOT covered by the catalog — sideloaded
    // third parties. Dedup against catalog names already emitted.
    let catalog_names: Vec<&str> = catalog.iter().map(|e| e.target_software.as_str()).collect();
    let mut extra: Vec<String> = loaded
        .iter()
        .map(|l| l.software.clone())
        .chain(installed_on_disk.iter().cloned())
        .filter(|s| !catalog_names.contains(&s.as_str()))
        .collect();
    extra.sort();
    extra.dedup();
    for software in extra {
        let live = loaded.iter().find(|l| l.software == software);
        let status = if live.is_some() {
            PluginLoadStatus::Loaded
        } else {
            PluginLoadStatus::InstalledNotLoaded
        };
        rows.push(PluginDetailOutput {
            name: software.clone(),
            catalog: None,
            installed_version: live.map(|l| l.semver.clone()),
            target_compat: live.map(|l| l.target_compat.clone()),
            orca_compat: live.map(|l| l.orca_compat.clone()),
            tools: live.map(|l| l.tools.clone()).unwrap_or_default(),
            status,
            sideloaded: true,
        });
    }

    rows
}

/// Software names of every executable plugin currently present in the install
/// dir (the filename is the `target_software`). Empty on non-unix, where the
/// subprocess loader is unavailable.
fn installed_software_on_disk() -> Vec<String> {
    #[cfg(not(unix))]
    {
        Vec::new()
    }
    #[cfg(unix)]
    {
        let Some(dir) = install_dir() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| is_executable_plugin(&e.path()))
            .filter_map(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
            })
            .collect()
    }
}

// ── plugin.create{action=install|invoke} ────────────────────────────────────

/// The `plugin.create` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginCreateAction {
    /// Install a plugin (sideload `--file` or catalog `--name`).
    Install,
    /// Invoke a loaded plugin verb by name (the generic mesh seam).
    Invoke,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginCreateArgs {
    /// Which create action to run: `install` or `invoke`.
    #[arg(long, value_enum)]
    pub action: PluginCreateAction,
    /// `install`: absolute path to an executable plugin to **sideload**.
    /// Mutually exclusive with `name`.
    #[arg(long)]
    #[serde(default)]
    pub file: Option<String>,
    /// `install`: catalog name to auto-download + install from its GitHub
    /// release. Mutually exclusive with `file`.
    #[arg(long)]
    #[serde(default)]
    pub name: Option<String>,
    /// `install` with `--name`: explicit plugin version/tag. Omit for newest.
    #[arg(long)]
    #[serde(default)]
    pub version: Option<String>,
    /// `install` with `--name` and no `--version`: include pre-release tags.
    #[arg(long, default_value_t = false)]
    #[serde(default)]
    pub prerelease: bool,
    /// `invoke`: fully-qualified verb of a loaded plugin, e.g.
    /// `proxmox.put_set_timezone`.
    #[arg(long)]
    #[serde(default)]
    pub tool: Option<String>,
    /// `invoke`: argument object forwarded verbatim to the plugin verb. On the
    /// CLI, pass a JSON object string (`--args '{"node":"frigg"}'`).
    #[serde(default)]
    #[arg(long, default_value = "{}", value_parser = parse_json_object)]
    pub args: serde_json::Value,
}

/// `plugin.create` payload — one variant per `action`.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(untagged)]
pub enum PluginCreateOutput {
    Install(PluginInstallOutput),
    Invoke(PluginInvokeOutput),
}

/// Create a plugin artifact. `action=install` sideloads (`--file`) or
/// catalog-installs (`--name`) a plugin, registering its tools live.
/// `action=invoke` runs a loaded plugin verb by name — the static `remote_ok`
/// core seam that makes any peer's dynamically-loaded plugin verbs reachable
/// over the mesh: call it with `peer: <host>` and the universal peer-dispatch
/// stanza relays it to the peer, which dispatches the named verb through its own
/// loaded-plugin registry. Discover verb names with `plugin.detail`/`plugin.list`.
#[orca_tool(domain = "plugin", verb = "create")]
async fn plugin_create(args: PluginCreateArgs, ctx: &ToolCtx) -> Result<PluginCreateOutput> {
    match args.action {
        PluginCreateAction::Install => {
            let install = PluginInstallArgs {
                file: args.file,
                name: args.name,
                version: args.version,
                prerelease: args.prerelease,
            };
            Ok(PluginCreateOutput::Install(
                plugin_install(install, ctx).await?,
            ))
        }
        PluginCreateAction::Invoke => {
            let tool = args
                .tool
                .ok_or_else(|| anyhow::anyhow!("`tool` is required for action=invoke"))?;
            let invoke = PluginInvokeArgs {
                tool,
                args: args.args,
            };
            Ok(PluginCreateOutput::Invoke(
                plugin_invoke(invoke, ctx).await?,
            ))
        }
    }
}

// ── plugin.invoke ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginInvokeArgs {
    /// Fully-qualified verb of a loaded plugin, e.g. `proxmox.put_set_timezone`.
    pub tool: String,
    /// Argument object forwarded verbatim to the plugin verb. On the CLI, pass a
    /// JSON object string (`--args '{"node":"frigg"}'`); defaults to `{}`.
    #[serde(default)]
    #[arg(long, default_value = "{}", value_parser = parse_json_object)]
    pub args: serde_json::Value,
}

/// Parse a CLI `--args` string as a JSON object. Rejects non-object JSON so a
/// plugin verb always receives a well-formed argument map.
fn parse_json_object(s: &str) -> std::result::Result<serde_json::Value, String> {
    let v: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}"))?;
    if !v.is_object() {
        return Err("expected a JSON object".to_string());
    }
    Ok(v)
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginInvokeOutput {
    /// The verb that was invoked.
    pub tool: String,
    /// The plugin verb's return value, verbatim.
    pub result: serde_json::Value,
}

/// Invoke a loaded plugin verb by name — the generic seam that makes plugin
/// verbs reachable over the mesh. A client attached to one daemon cannot NAME a
/// peer's dynamically-loaded plugin verbs (they register only on that peer's
/// tool surface), but `plugin.invoke` is a static, `remote_ok` core tool present
/// on every daemon. Call it with `peer: <host>` and the universal peer-dispatch
/// stanza relays THIS tool to the peer, which runs it locally and dispatches the
/// named verb through its own loaded-plugin registry. Discover verb names with
/// `plugin.detail` / `plugin.list` (also `peer`-dispatchable).
///
/// Restricted to plugin-owned verbs: we verify a loaded plugin owns `tool`
/// before dispatching, so this cannot be used to reach static core tools and
/// bypass their own `remote_ok`/role gates.
async fn plugin_invoke(args: PluginInvokeArgs, ctx: &ToolCtx) -> Result<PluginInvokeOutput> {
    let owned = plugin_loader::loaded_tool_defs()
        .iter()
        .any(|d| d.name == args.tool);
    if !owned {
        bail!(
            "no loaded plugin owns verb '{}' — run `plugin.list`/`plugin.detail` (optionally with `peer`) to see available verbs",
            args.tool
        );
    }
    let result = plugin_loader::dispatch(&args.tool, args.args, ctx).await?;
    Ok(PluginInvokeOutput {
        tool: args.tool,
        result,
    })
}

// ── plugin.install ───────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginInstallArgs {
    /// Absolute path to an executable plugin to **sideload**. Mutually exclusive
    /// with `name`. The plugin is spawned and handshaked before the file is
    /// accepted.
    #[arg(long)]
    pub file: Option<String>,
    /// Catalog name to auto-download + install from its GitHub release,
    /// selecting the asset that matches this daemon's target triple. Mutually
    /// exclusive with `file`.
    #[arg(long)]
    pub name: Option<String>,
    /// With `--name`: explicit plugin version/tag to install (e.g. `0.1.1-rc.2`).
    /// Omit for the newest release.
    #[arg(long)]
    pub version: Option<String>,
    /// With `--name` and no `--version`: include pre-release (`-rc`) tags when
    /// picking the newest release. Off by default (stable only).
    #[arg(long, default_value_t = false)]
    pub prerelease: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstallOutput {
    /// The installed plugin's `target_software`.
    pub software: String,
    /// The installed plugin's semver.
    pub version: String,
    /// Tools registered live by the install.
    pub tools: Vec<String>,
    /// Absolute path the plugin executable was copied to in the install dir.
    pub installed_path: String,
    /// True — sideload registers tools immediately, no restart needed.
    pub loaded_live: bool,
}

/// Install a plugin. Two modes:
///
/// * `--file <path>` — **sideload**: spawn the executable and complete the
///   `plugin-proto` wire handshake FIRST; only on a clean handshake copy it into
///   the install dir under a deterministic name and register its tools live (no
///   restart). On a handshake failure the install is refused and nothing is
///   copied.
/// * `--name <catalog-name>` — auto-download from the catalog and install.
async fn plugin_install(args: PluginInstallArgs, _ctx: &ToolCtx) -> Result<PluginInstallOutput> {
    if args.file.is_some() && args.name.is_some() {
        bail!("pass exactly one of --file (sideload) or --name (catalog install), not both");
    }

    if let Some(name) = &args.name {
        return install_from_catalog(name, args.version.as_deref(), args.prerelease, _ctx).await;
    }

    let Some(file) = &args.file else {
        bail!(
            "provide --file <path> to sideload an executable plugin, or --name <catalog-name> to install from GitHub"
        );
    };

    let src = Path::new(file);
    if !src.is_file() {
        bail!("no such file: {file}");
    }

    #[cfg(not(unix))]
    {
        let _ = src;
        bail!("subprocess plugins require unix");
    }

    #[cfg(unix)]
    {
        // ── Spawn + handshake FIRST, from the source path — refuse before
        //    touching the install dir. A failed handshake returns the loader's
        //    clean error and installs nothing.
        // Trust-on-first-use: the id is learned from this handshake and recorded
        // as the install filename below, so there is no prior id to validate.
        let report = plugin_loader::spawn_plugin(src, None)
            .with_context(|| format!("refusing to install {file}: plugin handshake failed"))?;
        apply_plugin_schema(&report);

        // ── Handshake passed: the plugin is registered live. Persist it so the
        //    startup scan re-spawns it next boot. Copy under the deterministic
        //    name and mark it executable.
        let dir = install_dir().context("cannot resolve plugin install dir (no orca_home)")?;
        files::ops::mkdir_p(&dir)?;
        let dest = dir.join(install_filename(&report.software));
        // If we're sideloading a file already inside the install dir under its
        // canonical name, skip the copy (copying a file onto itself errors).
        if src.canonicalize().ok() != dest.canonicalize().ok() {
            // Copy to a same-dir temp then atomically rename over `dest`, so an
            // in-place upgrade of a still-running plugin binary can't hit ETXTBSY.
            // See [[project-orca-plugin-rollout-defects]].
            let tmp = dir.join(format!(".{}.incoming", install_filename(&report.software)));
            std::fs::copy(src, &tmp)
                .with_context(|| format!("failed to copy plugin into {}", tmp.display()))?;
            if let Err(e) = std::fs::rename(&tmp, &dest) {
                if let Err(rm) = std::fs::remove_file(&tmp) {
                    tracing::warn!(path = %tmp.display(), error = %rm, "could not remove temp plugin artifact");
                }
                return Err(anyhow::Error::new(e).context(format!(
                    "failed to install plugin binary to {}",
                    dest.display()
                )));
            }
        }
        make_executable(&dest)
            .with_context(|| format!("failed to mark {} executable", dest.display()))?;

        tracing::info!(
            plugin = %report.software,
            version = %report.semver,
            path = %dest.display(),
            "sideloaded plugin (handshake passed, registered live)"
        );

        Ok(PluginInstallOutput {
            software: report.software,
            version: report.semver,
            tools: report.tools,
            installed_path: dest.display().to_string(),
            loaded_live: true,
        })
    }
}

/// Install a first-party plugin from its GitHub release (the `--name` path).
///
/// Resolves the catalog entry, downloads the release asset matching THIS
/// daemon's target triple (via [`crate::plugin_fetch`]), writes it to the
/// install dir, then spawns + handshakes it exactly like sideload and registers
/// live. Persistent: the startup scan re-spawns it on the next boot.
async fn install_from_catalog(
    name: &str,
    version: Option<&str>,
    prerelease: bool,
    ctx: &ToolCtx,
) -> Result<PluginInstallOutput> {
    let entry = catalog_resolved()
        .await
        .into_iter()
        .find(|e| e.name == name || e.target_software == name)
        .with_context(|| {
            format!("'{name}' is not in the plugin catalog (see `plugin.list` for known plugins)")
        })?;
    if entry.status != "available" {
        bail!(
            "plugin '{name}' is '{}', not installable from the catalog yet \
             (no published release artifact)",
            entry.status
        );
    }

    // Direct fetch from GitHub. On failure, when this host holds NO github_token
    // (so a private/rate-limited asset is unreachable and retrying won't help),
    // fall back to a paired secure peer that DOES hold one — the same
    // delegate-on-miss the orca binary self-update uses. The token never leaves
    // the holder; we get back verified bytes for our own triple.
    // See [[github-token-proxy-delegate-on-miss]].
    let fetched = match crate::plugin_fetch::fetch(
        &entry.target_software,
        &entry.repo_url,
        version,
        prerelease,
    )
    .await
    {
        Ok(f) => f,
        Err(e) if crate::update::resolve_github_token().is_empty() => {
            delegate_plugin_fetch(&entry, version, prerelease, ctx)
                .await
                .with_context(|| format!("direct plugin fetch failed ({e}); delegate"))?
        }
        Err(e) => return Err(e),
    };

    let dir = install_dir().context("cannot resolve plugin install dir (no orca_home)")?;
    files::ops::mkdir_p(&dir)?;
    let dest = dir.join(install_filename(&entry.target_software));
    // Write to a temp path in the same dir, then atomically rename over `dest`.
    // A plain write-over fails with ETXTBSY when `dest` is the currently-running
    // plugin binary (in-place upgrade); rename replaces the directory entry while
    // the old inode stays mapped by the running process until it exits. Same-dir
    // temp keeps the rename atomic (no cross-filesystem copy).
    // See [[project-orca-plugin-rollout-defects]].
    let tmp = dir.join(format!(
        ".{}.incoming",
        install_filename(&entry.target_software)
    ));
    std::fs::write(&tmp, &fetched.bytes)
        .with_context(|| format!("failed to write plugin to {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        if let Err(rm) = std::fs::remove_file(&tmp) {
            tracing::warn!(path = %tmp.display(), error = %rm, "could not remove temp plugin artifact");
        }
        return Err(anyhow::Error::new(e).context(format!(
            "failed to install plugin binary to {}",
            dest.display()
        )));
    }

    #[cfg(not(unix))]
    {
        let _ = &dest;
        bail!("subprocess plugins require unix");
    }

    #[cfg(unix)]
    {
        make_executable(&dest)
            .with_context(|| format!("failed to mark {} executable", dest.display()))?;

        // Spawn + handshake from the installed path. On a failure remove the file
        // so a broken artifact isn't left for the next startup scan to trip on.
        // The catalog target_software is the authoritative id (== dest filename).
        let report = match plugin_loader::spawn_plugin(&dest, Some(&entry.target_software)) {
            Ok(r) => r,
            Err(e) => {
                if let Err(rm) = std::fs::remove_file(&dest) {
                    tracing::warn!(path = %dest.display(), error = %rm, "could not remove rejected plugin artifact");
                }
                return Err(e.context(format!(
                    "downloaded {} but it failed the plugin handshake; not installed",
                    fetched.asset
                )));
            }
        };
        apply_plugin_schema(&report);

        tracing::info!(
            plugin = %report.software,
            version = %report.semver,
            asset = %fetched.asset,
            path = %dest.display(),
            "installed plugin from catalog (handshake passed, registered live)"
        );

        Ok(PluginInstallOutput {
            software: report.software,
            version: report.semver,
            tools: report.tools,
            installed_path: dest.display().to_string(),
            loaded_live: true,
        })
    }
}

// ── plugin.serve_asset — delegate-on-miss holder side ────────────────────────
//
// Peer-dispatchable. A host whose `github_token` secret is empty calls this on
// a paired secure peer that DOES hold a token; the holder fetches the plugin
// release asset from GitHub **for the caller's target triple**, verifies the
// sha256, and returns the bytes base64-encoded for the JSON-only wire
// transport. The token never leaves the holder. Mirrors
// `system.serve_release` for the orca binary. See
// [[github-token-proxy-delegate-on-miss]].

/// Args for [`plugin_serve_asset`].
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct PluginServeAssetArgs {
    /// Plugin name (also its `target_software` and release-asset prefix).
    #[arg(long)]
    pub name: String,
    /// The plugin repo web URL (catalog `repoUrl`), e.g.
    /// `https://github.com/argyle-labs/sonarr`.
    #[arg(long)]
    pub repo_url: String,
    /// Rust target triple of the REQUESTER (e.g. `x86_64-unknown-linux-musl`).
    /// The holder may be a different arch, so the caller MUST specify the asset
    /// it needs.
    #[arg(long)]
    pub target: String,
    /// Explicit version/tag (leading `v` optional). Omit for the newest release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub version: Option<String>,
    /// Include prerelease (`-rc`) tags when resolving the newest release.
    #[serde(default)]
    #[arg(long)]
    pub prerelease: bool,
}

/// Result of [`plugin_serve_asset`]. `asset_b64` is base64-STANDARD of the raw
/// plugin executable; `sha256` is the hex digest the holder verified (callers
/// MUST re-verify after decode before installing).
#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct PluginServeAssetOutput {
    pub asset_b64: String,
    pub sha256: String,
    pub version: String,
}

/// Serve a plugin release asset from GitHub on behalf of a peer that lacks the
/// `github_token` secret. Fetches the asset for the requested `target` using
/// the holder's own token, then returns the verified bytes base64-encoded.
#[orca_tool(domain = "plugin", verb = "serve_asset")]
async fn plugin_serve_asset(
    args: PluginServeAssetArgs,
    _ctx: &ToolCtx,
) -> Result<PluginServeAssetOutput> {
    let fetched = crate::plugin_fetch::fetch_for_target(
        &args.name,
        &args.repo_url,
        args.version.as_deref(),
        args.prerelease,
        &args.target,
    )
    .await?;
    Ok(PluginServeAssetOutput {
        asset_b64: utils::encoding::base64_encode(&fetched.bytes),
        sha256: fetched.sha256,
        version: fetched.version,
    })
}

/// Delegate-on-miss (caller side): when this host has no `github_token`, ask a
/// paired secure peer that holds one to fetch the plugin asset for OUR triple.
/// Returns the verified [`crate::plugin_fetch::FetchedPlugin`] as if fetched
/// locally, or an error aggregating every candidate peer's failure.
async fn delegate_plugin_fetch(
    entry: &CatalogEntry,
    version: Option<&str>,
    prerelease: bool,
    ctx: &ToolCtx,
) -> Result<crate::plugin_fetch::FetchedPlugin> {
    let target = crate::update::build_target().to_string();
    if target == "unknown-target" {
        bail!("this daemon has no baked build target; cannot delegate a plugin fetch");
    }

    let conn = db::open_default().context("open orca.db for peer enumeration")?;
    let present: Vec<db::pod::peerdb::PeerRow> = db::pod::peerdb::list_peers(&conn)
        .context("list paired peers")?
        .into_iter()
        .filter(|p| p.departed_at.is_none())
        .collect();
    let candidates: Vec<&db::pod::peerdb::PeerRow> =
        present.iter().filter(|p| p.peer_secure).collect();
    if candidates.is_empty() {
        let insecure: Vec<(String, String)> = present
            .iter()
            .filter(|p| !p.peer_secure)
            .map(|p| (p.peer_id.clone(), p.peer_hostname.clone()))
            .collect();
        bail!(crate::commands::no_secure_peer_message(&insecure));
    }

    // Surface a clear error if no transport is registered, rather than letting
    // the macro-emitted peer_dispatch fail per-peer.
    ctx.service::<Arc<dyn RemoteExec>>()
        .context("no RemoteExec transport registered for delegate fetch")?;

    let mut errs: Vec<String> = Vec::new();
    for peer in &candidates {
        let args = PluginServeAssetArgs {
            name: entry.target_software.clone(),
            repo_url: entry.repo_url.clone(),
            target: target.clone(),
            version: version.map(str::to_string),
            prerelease,
        };
        // Setting ctx.peer triggers the macro-emitted peer_dispatch stanza in
        // `plugin_serve_asset`, routing the call through RemoteExec to the peer.
        let peered = ctx.clone().with_peer(peer.peer_hostname.clone());
        let out = match plugin_serve_asset(args, &peered).await {
            Ok(o) => o,
            Err(e) => {
                errs.push(format!("{}: {e}", peer.peer_hostname));
                continue;
            }
        };
        let bytes = match utils::encoding::base64_decode(&out.asset_b64) {
            Ok(b) => b,
            Err(e) => {
                errs.push(format!("{}: base64 decode: {e}", peer.peer_hostname));
                continue;
            }
        };
        if let Err(e) = crate::update::verify_sha256(&bytes, &out.sha256) {
            errs.push(format!("{}: sha256 verify: {e}", peer.peer_hostname));
            continue;
        }
        tracing::info!(
            plugin = %entry.target_software,
            peer = %peer.peer_hostname,
            version = %out.version,
            "fetched plugin asset via delegate-on-miss (token held by peer)"
        );
        return Ok(crate::plugin_fetch::FetchedPlugin {
            bytes,
            version: out.version,
            asset: asset_label(&entry.target_software, &target),
            sha256: out.sha256,
        });
    }
    bail!(
        "all {} delegate peers failed: {}",
        candidates.len(),
        errs.join("; ")
    );
}

/// A human-facing asset label for logging when the real filename came from a
/// peer (we know the name+triple but not the exact resolved version string yet).
fn asset_label(name: &str, triple: &str) -> String {
    format!("{name}-<peer-served>-{triple}")
}

// ── plugin.delete{action=uninstall} ──────────────────────────────────────────

/// The `plugin.delete` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginDeleteAction {
    /// Remove a plugin: delete its executable and unregister its tools.
    #[default]
    Uninstall,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginUninstallArgs {
    /// Which delete action to run. Defaults to `uninstall`.
    #[arg(long, value_enum, default_value = "uninstall")]
    #[serde(default)]
    pub action: PluginDeleteAction,
    /// `target_software` name of the plugin to remove, e.g. `"jellyfin"`.
    #[arg(long)]
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginUninstallOutput {
    /// The plugin removed.
    pub software: String,
    /// True if a file was deleted from the install dir.
    pub removed_from_disk: bool,
    /// True if the plugin was unregistered from the live tool registry.
    pub unloaded: bool,
}

/// Remove a plugin: delete its executable from the install dir and unregister
/// its tools from the live registry. Idempotent — reports what it actually
/// removed.
#[orca_tool(domain = "plugin", verb = "delete")]
async fn plugin_uninstall(
    args: PluginUninstallArgs,
    _ctx: &ToolCtx,
) -> Result<PluginUninstallOutput> {
    let PluginDeleteAction::Uninstall = args.action;
    let software = args.name.trim();
    if software.is_empty() {
        bail!("--name is required");
    }

    let removed_from_disk = if let Some(dir) = install_dir() {
        let path = dir.join(install_filename(software));
        if path.is_file() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            true
        } else {
            false
        }
    } else {
        false
    };

    let unloaded = plugin_loader::unload_plugin(software) > 0;

    if !removed_from_disk && !unloaded {
        bail!("plugin '{software}' is not installed or loaded");
    }

    tracing::info!(plugin = %software, removed_from_disk, unloaded, "uninstalled plugin");

    Ok(PluginUninstallOutput {
        software: software.to_string(),
        removed_from_disk,
        unloaded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(software: &str) -> plugin_loader::LoadedPluginInfo {
        plugin_loader::LoadedPluginInfo {
            software: software.to_string(),
            semver: "1.2.3".to_string(),
            target_compat: ">=1.0.0".to_string(),
            orca_compat: ">=0.1.0".to_string(),
            tools: vec![format!("{software}.list"), format!("{software}.detail")],
        }
    }

    fn entry(name: &str, status: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_string(),
            target_software: name.to_string(),
            repo_url: format!("https://github.com/argyle-labs/{name}"),
            docs_url: format!("https://github.com/argyle-labs/{name}#readme"),
            status: status.to_string(),
        }
    }

    #[test]
    fn asset_label_names_peer_served_artifact() {
        // Peer-served fetches know name + triple but not the exact resolved
        // version string, so the log label marks the provenance explicitly.
        assert_eq!(
            asset_label("sonarr", "x86_64-unknown-linux-musl"),
            "sonarr-<peer-served>-x86_64-unknown-linux-musl"
        );
    }

    // ── embedded catalog ──────────────────────────────────────────────────────

    #[test]
    fn embedded_catalog_parses_and_is_nonempty() {
        let entries = catalog().expect("embedded catalog must parse");
        assert!(!entries.is_empty());
        // Every entry has a name and a github repo url; name == target_software
        // is the invariant the loader relies on.
        for e in &entries {
            assert!(!e.name.is_empty());
            assert_eq!(e.name, e.target_software);
            assert!(e.repo_url.starts_with("https://github.com/"));
            assert!(e.status == "available" || e.status == "unreleased" || e.status == "planned");
        }
    }

    #[test]
    fn embedded_catalog_has_known_entries() {
        let entries = catalog().unwrap();
        assert!(entries.iter().any(|e| e.name == "jellyfin"));
        assert!(entries.iter().any(|e| e.name == "proxmox"));
        assert!(entries.iter().any(|e| e.status == "available"));
        assert!(entries.iter().any(|e| e.status == "unreleased"));
    }

    // ── install_filename ──────────────────────────────────────────────────────

    #[test]
    fn install_filename_is_the_bare_software_name() {
        for name in ["jellyfin", "proxmox", "calibre-web", "zwave-js-ui"] {
            assert_eq!(install_filename(name), name);
        }
    }

    // ── PluginLoadStatus serde ────────────────────────────────────────────────

    #[test]
    fn load_status_serializes_camel_case() {
        assert_eq!(
            serde_json::to_string(&PluginLoadStatus::Loaded).unwrap(),
            "\"loaded\""
        );
        assert_eq!(
            serde_json::to_string(&PluginLoadStatus::NotInstalled).unwrap(),
            "\"notInstalled\""
        );
        assert_eq!(
            serde_json::to_string(&PluginLoadStatus::InstalledNotLoaded).unwrap(),
            "\"installedNotLoaded\""
        );
    }

    // ── build_plugin_list_rows ────────────────────────────────────────────────

    #[test]
    fn rows_preserve_catalog_order_and_status() {
        let catalog = vec![entry("jellyfin", "available"), entry("plex", "available")];
        let loaded_live = vec![loaded("jellyfin")];
        let on_disk = vec!["plex".to_string()];

        let rows = build_plugin_list_rows(&catalog, &loaded_live, &on_disk);
        assert_eq!(rows.len(), 2);

        // jellyfin: loaded live.
        assert_eq!(rows[0].name, "jellyfin");
        assert_eq!(rows[0].status, PluginLoadStatus::Loaded);
        assert_eq!(rows[0].installed_version.as_deref(), Some("1.2.3"));
        assert_eq!(rows[0].tools.len(), 2);
        assert!(!rows[0].sideloaded);
        assert!(rows[0].catalog.is_some());

        // plex: on disk but not loaded.
        assert_eq!(rows[1].name, "plex");
        assert_eq!(rows[1].status, PluginLoadStatus::InstalledNotLoaded);
        assert!(rows[1].installed_version.is_none());
        assert!(rows[1].tools.is_empty());
    }

    #[test]
    fn parse_json_object_accepts_object_and_rejects_non_object() {
        assert_eq!(
            parse_json_object(r#"{"node":"frigg"}"#).unwrap(),
            serde_json::json!({"node": "frigg"})
        );
        assert_eq!(parse_json_object("{}").unwrap(), serde_json::json!({}));
        assert!(parse_json_object("[1,2]").is_err());
        assert!(parse_json_object("\"x\"").is_err());
        assert!(parse_json_object("not json").is_err());
    }

    #[test]
    fn plugin_create_is_registered_remote_ok_and_admin() {
        // The generic mesh seam (action=invoke) folds into `plugin.create`, which
        // must be present, peer-dispatchable, and gated to admin (verb "create"
        // is not read-shaped, so it default-denies).
        assert!(
            dispatch::remote_ok_names().contains(&"plugin.create"),
            "plugin.create must be in the remote_ok allowlist for pod/exec"
        );
        assert_eq!(dispatch::required_role("plugin.create"), Some("admin"));
    }

    #[test]
    fn rows_report_not_installed_for_bare_catalog() {
        let catalog = vec![entry("proxmox", "available")];
        let rows = build_plugin_list_rows(&catalog, &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PluginLoadStatus::NotInstalled);
        assert!(!rows[0].sideloaded);
    }

    #[test]
    fn sideloaded_plugins_appended_sorted_and_deduped() {
        let catalog = vec![entry("jellyfin", "available")];
        // "zzz" loaded live and on disk (dup); "aaa" only on disk.
        let loaded_live = vec![loaded("zzz")];
        let on_disk = vec!["zzz".to_string(), "aaa".to_string()];

        let rows = build_plugin_list_rows(&catalog, &loaded_live, &on_disk);
        // jellyfin + aaa + zzz (deduped), sideloaded tail sorted.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "jellyfin");
        assert_eq!(rows[1].name, "aaa");
        assert!(rows[1].sideloaded);
        assert_eq!(rows[1].status, PluginLoadStatus::InstalledNotLoaded);
        assert_eq!(rows[2].name, "zzz");
        assert!(rows[2].sideloaded);
        assert_eq!(rows[2].status, PluginLoadStatus::Loaded);
        assert_eq!(rows[2].installed_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn catalog_names_are_never_sideloaded() {
        // A catalog plugin present both in catalog and on disk must not also
        // appear in the sideloaded tail.
        let catalog = vec![entry("docker", "available")];
        let on_disk = vec!["docker".to_string()];
        let rows = build_plugin_list_rows(&catalog, &[], &on_disk);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].sideloaded);
    }

    // ── install_dir / installed_software_on_disk (tempdir) ────────────────────

    #[test]
    #[serial_test::serial(env)]
    fn install_dir_derives_from_orca_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        let dir = install_dir().expect("orca_home set");
        assert_eq!(dir, tmp.path().join("plugins"));
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial(env)]
    fn installed_software_on_disk_scans_executables_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        // Executable plugin files are found by their bare name; a non-executable
        // file (README) in the same dir is ignored.
        for name in ["jellyfin", "proxmox"] {
            let p = plugins.join(install_filename(name));
            std::fs::write(&p, b"x").unwrap();
            make_executable(&p).unwrap();
        }
        std::fs::write(plugins.join("README.md"), b"x").unwrap();

        let mut found = installed_software_on_disk();
        found.sort();
        assert_eq!(found, vec!["jellyfin".to_string(), "proxmox".to_string()]);
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn installed_software_on_disk_empty_when_no_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // plugins/ never created.
        assert!(installed_software_on_disk().is_empty());
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn scan_and_load_empty_when_dir_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        let (loaded, failed) = scan_and_load();
        assert!(loaded.is_empty());
        assert!(failed.is_empty());
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_plugin_detects_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("peacock");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable_plugin(&exe));

        let plain = tmp.path().join("notes.txt");
        std::fs::write(&plain, b"x").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable_plugin(&plain));

        assert!(!is_executable_plugin(tmp.path())); // a directory
    }

    // ── CatalogEntry serde (camelCase) ────────────────────────────────────────

    #[test]
    fn catalog_entry_serializes_camel_case_keys() {
        let e = entry("jellyfin", "available");
        let json = serde_json::to_string(&e).unwrap();
        // camelCase rename must be applied to the multi-word fields.
        assert!(json.contains("\"targetSoftware\":\"jellyfin\""), "{json}");
        assert!(
            json.contains("\"repoUrl\":\"https://github.com/argyle-labs/jellyfin\""),
            "{json}"
        );
        assert!(json.contains("\"docsUrl\":"), "{json}");
        assert!(json.contains("\"status\":\"available\""), "{json}");
        // snake_case aliases must NOT leak into the wire form.
        assert!(!json.contains("target_software"), "{json}");
        assert!(!json.contains("repo_url"), "{json}");
    }

    #[test]
    fn catalog_entry_deserializes_from_camel_case() {
        let src = r#"{
            "name": "sonarr",
            "targetSoftware": "sonarr",
            "repoUrl": "https://github.com/argyle-labs/sonarr",
            "docsUrl": "https://github.com/argyle-labs/sonarr#readme",
            "status": "unreleased"
        }"#;
        let e: CatalogEntry = serde_json::from_str(src).unwrap();
        assert_eq!(e.name, "sonarr");
        assert_eq!(e.target_software, "sonarr");
        assert_eq!(e.repo_url, "https://github.com/argyle-labs/sonarr");
        assert_eq!(e.status, "unreleased");
    }

    #[test]
    fn catalog_entry_round_trips_through_serde() {
        let original = entry("proxmox", "planned");
        let json = serde_json::to_string(&original).unwrap();
        let back: CatalogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, original.name);
        assert_eq!(back.target_software, original.target_software);
        assert_eq!(back.repo_url, original.repo_url);
        assert_eq!(back.docs_url, original.docs_url);
        assert_eq!(back.status, original.status);
    }

    // ── CatalogFile parses the plugins array wrapper ──────────────────────────

    #[test]
    fn catalog_file_deserializes_plugins_wrapper() {
        let src = r#"{"plugins":[{
            "name":"a","targetSoftware":"a","repoUrl":"https://github.com/x/a",
            "docsUrl":"https://x/a","status":"available"}]}"#;
        let f: CatalogFile = serde_json::from_str(src).unwrap();
        assert_eq!(f.plugins.len(), 1);
        assert_eq!(f.plugins[0].name, "a");
    }

    // ── PluginLoadStatus deserialize (camelCase) ──────────────────────────────

    #[test]
    fn load_status_deserializes_camel_case() {
        let loaded: PluginLoadStatus = serde_json::from_str("\"loaded\"").unwrap();
        assert_eq!(loaded, PluginLoadStatus::Loaded);
        let not: PluginLoadStatus = serde_json::from_str("\"notInstalled\"").unwrap();
        assert_eq!(not, PluginLoadStatus::NotInstalled);
        let inl: PluginLoadStatus = serde_json::from_str("\"installedNotLoaded\"").unwrap();
        assert_eq!(inl, PluginLoadStatus::InstalledNotLoaded);
    }

    // ── PluginCreateAction / PluginDeleteAction serde ─────────────────────────

    #[test]
    fn create_action_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&PluginCreateAction::Install).unwrap(),
            "\"install\""
        );
        assert_eq!(
            serde_json::to_string(&PluginCreateAction::Invoke).unwrap(),
            "\"invoke\""
        );
        let a: PluginCreateAction = serde_json::from_str("\"install\"").unwrap();
        assert_eq!(a, PluginCreateAction::Install);
    }

    #[test]
    fn delete_action_defaults_to_uninstall_and_serializes_snake_case() {
        assert_eq!(PluginDeleteAction::default(), PluginDeleteAction::Uninstall);
        assert_eq!(
            serde_json::to_string(&PluginDeleteAction::Uninstall).unwrap(),
            "\"uninstall\""
        );
        let a: PluginDeleteAction = serde_json::from_str("\"uninstall\"").unwrap();
        assert_eq!(a, PluginDeleteAction::Uninstall);
    }

    // ── PluginListRow::from_detail projection ─────────────────────────────────

    #[test]
    fn list_row_projects_detail_to_thin_shape() {
        let detail = PluginDetailOutput {
            name: "jellyfin".to_string(),
            catalog: Some(entry("jellyfin", "available")),
            installed_version: Some("9.9.9".to_string()),
            target_compat: Some(">=1.0.0".to_string()),
            orca_compat: Some(">=0.1.0".to_string()),
            tools: vec!["jellyfin.list".to_string(), "jellyfin.detail".to_string()],
            status: PluginLoadStatus::Loaded,
            sideloaded: false,
        };
        let row = PluginListRow::from_detail(&detail);
        assert_eq!(row.name, "jellyfin");
        assert_eq!(row.installed_version.as_deref(), Some("9.9.9"));
        // The heavy tools array collapses to a count on the thin row.
        assert_eq!(row.tool_count, 2);
        assert_eq!(row.status, PluginLoadStatus::Loaded);
        assert!(!row.sideloaded);
        assert!(row.catalog.is_some());
    }

    // ── PluginListOutput skip_serializing_if on paging fields ─────────────────

    #[test]
    fn list_output_omits_absent_paging_fields() {
        let out = PluginListOutput {
            plugins: Vec::new(),
            next_cursor: None,
            total: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("nextCursor"), "{json}");
        assert!(!json.contains("total"), "{json}");
        assert!(json.contains("\"plugins\":[]"), "{json}");
    }

    #[test]
    fn list_output_includes_present_paging_fields() {
        let out = PluginListOutput {
            plugins: Vec::new(),
            next_cursor: Some("abc".to_string()),
            total: Some(7),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"nextCursor\":\"abc\""), "{json}");
        assert!(json.contains("\"total\":7"), "{json}");
    }

    // ── build_plugin_list_rows: extra edge cases ──────────────────────────────

    #[test]
    fn rows_empty_catalog_and_no_plugins_is_empty() {
        assert!(build_plugin_list_rows(&[], &[], &[]).is_empty());
    }

    #[test]
    fn rows_sideloaded_loaded_only_not_on_disk() {
        // A plugin loaded live but absent from disk still surfaces as a
        // sideloaded row, carrying its live version + tools.
        let loaded_live = vec![loaded("thirdparty")];
        let rows = build_plugin_list_rows(&[], &loaded_live, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "thirdparty");
        assert!(rows[0].sideloaded);
        assert_eq!(rows[0].status, PluginLoadStatus::Loaded);
        assert_eq!(rows[0].installed_version.as_deref(), Some("1.2.3"));
        assert!(rows[0].catalog.is_none());
    }

    // ── async guard branches (no side effects reached) ────────────────────────

    fn guard_ctx(tmp: &tempfile::TempDir) -> ToolCtx {
        // SAFETY: env-touching guard tests are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
        }
        let config = contract::config::Config::load().expect("Config::load under temp ORCA_HOME");
        ToolCtx::new(Arc::new(config))
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn install_rejects_both_file_and_name() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let args = PluginInstallArgs {
            file: Some("/tmp/x".to_string()),
            name: Some("jellyfin".to_string()),
            version: None,
            prerelease: false,
        };
        let err = plugin_install(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("exactly one"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn install_requires_file_or_name() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let args = PluginInstallArgs::default();
        let err = plugin_install(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("provide --file"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn install_rejects_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let missing = tmp.path().join("does-not-exist");
        let args = PluginInstallArgs {
            file: Some(missing.display().to_string()),
            name: None,
            version: None,
            prerelease: false,
        };
        let err = plugin_install(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("no such file"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn create_invoke_requires_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let args = PluginCreateArgs {
            action: PluginCreateAction::Invoke,
            file: None,
            name: None,
            version: None,
            prerelease: false,
            tool: None,
            args: serde_json::json!({}),
        };
        let err = plugin_create(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("`tool` is required"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn invoke_unknown_verb_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        // No loaded plugin owns this verb, so dispatch is refused before any
        // plugin subprocess is touched.
        let args = PluginInvokeArgs {
            tool: "definitely-not-a-loaded-plugin.nope".to_string(),
            args: serde_json::json!({}),
        };
        let err = plugin_invoke(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("no loaded plugin owns verb"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn uninstall_requires_nonempty_name() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let args = PluginUninstallArgs {
            action: PluginDeleteAction::Uninstall,
            name: "   ".to_string(),
        };
        let err = plugin_uninstall(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("--name is required"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn uninstall_reports_not_installed_for_unknown_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        // Fresh temp ORCA_HOME: nothing on disk, nothing loaded, so an unknown
        // name is neither removed nor unloaded and the tool reports as much.
        let args = PluginUninstallArgs {
            action: PluginDeleteAction::Uninstall,
            name: "no-such-plugin-xyz".to_string(),
        };
        let err = plugin_uninstall(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("not installed or loaded"),
            "unexpected: {err:#}"
        );
    }

    // ── uninstall removes an on-disk artifact (removed_from_disk branch) ───────

    #[tokio::test]
    #[cfg(unix)]
    #[serial_test::serial(env)]
    async fn uninstall_removes_on_disk_artifact() {
        // A plugin file present in the install dir (but never loaded live) is
        // removed from disk and the tool reports removed_from_disk=true /
        // unloaded=false. No subprocess is spawned — pure filesystem side effect.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let dir = install_dir().expect("orca_home set");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(install_filename("ghostplugin"));
        std::fs::write(&path, b"x").unwrap();
        make_executable(&path).unwrap();

        let out = plugin_uninstall(
            PluginUninstallArgs {
                action: PluginDeleteAction::Uninstall,
                name: "ghostplugin".to_string(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.software, "ghostplugin");
        assert!(out.removed_from_disk);
        assert!(!out.unloaded);
        assert!(!path.exists(), "artifact should be deleted from disk");
    }

    #[tokio::test]
    #[cfg(unix)]
    #[serial_test::serial(env)]
    async fn uninstall_trims_name_before_lookup() {
        // The name is trimmed: a padded, on-disk plugin name still resolves and
        // is removed (guards the `.trim()` + install_filename join path).
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let dir = install_dir().expect("orca_home set");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(install_filename("trimme"));
        std::fs::write(&path, b"x").unwrap();
        make_executable(&path).unwrap();

        let out = plugin_uninstall(
            PluginUninstallArgs {
                action: PluginDeleteAction::Uninstall,
                name: "  trimme  ".to_string(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(out.software, "trimme");
        assert!(out.removed_from_disk);
        assert!(!path.exists());
    }

    // ── plugin_create install dispatch guards (pre-side-effect bails) ──────────

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn create_install_rejects_both_file_and_name() {
        // action=install forwards to plugin_install, which bails before any fetch
        // or subprocess when both --file and --name are supplied.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let args = PluginCreateArgs {
            action: PluginCreateAction::Install,
            file: Some("/tmp/x".to_string()),
            name: Some("jellyfin".to_string()),
            version: None,
            prerelease: false,
            tool: None,
            args: serde_json::json!({}),
        };
        let err = plugin_create(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("exactly one"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn create_install_requires_file_or_name() {
        // action=install with neither --file nor --name bails before touching
        // the install dir or the network.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let args = PluginCreateArgs {
            action: PluginCreateAction::Install,
            file: None,
            name: None,
            version: None,
            prerelease: false,
            tool: None,
            args: serde_json::json!({}),
        };
        let err = plugin_create(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("provide --file"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn create_install_rejects_missing_file() {
        // action=install --file <missing> bails at the is_file() guard, before
        // any spawn/handshake.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let missing = tmp.path().join("nope-binary");
        let args = PluginCreateArgs {
            action: PluginCreateAction::Install,
            file: Some(missing.display().to_string()),
            name: None,
            version: None,
            prerelease: false,
            tool: None,
            args: serde_json::json!({}),
        };
        let err = plugin_create(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("no such file"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn create_invoke_unknown_verb_is_refused() {
        // action=invoke with a tool no loaded plugin owns is refused before any
        // dispatch to a plugin subprocess.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let args = PluginCreateArgs {
            action: PluginCreateAction::Invoke,
            file: None,
            name: None,
            version: None,
            prerelease: false,
            tool: Some("nope-plugin.nope".to_string()),
            args: serde_json::json!({}),
        };
        let err = plugin_create(args, &ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("no loaded plugin owns verb"),
            "unexpected: {err:#}"
        );
    }

    // ── loaded catalog rows carry live compat ranges ──────────────────────────

    #[test]
    fn loaded_catalog_row_carries_compat_ranges() {
        // A catalog entry that is loaded live must project the loader's compat
        // ranges onto the row (target_compat / orca_compat), not just the version.
        let catalog = vec![entry("jellyfin", "available")];
        let live = vec![loaded("jellyfin")];
        let rows = build_plugin_list_rows(&catalog, &live, &[]);
        assert_eq!(rows[0].target_compat.as_deref(), Some(">=1.0.0"));
        assert_eq!(rows[0].orca_compat.as_deref(), Some(">=0.1.0"));
    }

    #[test]
    fn not_loaded_catalog_row_has_no_compat_ranges() {
        // A catalog entry with no live info leaves the compat ranges absent.
        let catalog = vec![entry("plex", "available")];
        let rows = build_plugin_list_rows(&catalog, &[], &[]);
        assert!(rows[0].target_compat.is_none());
        assert!(rows[0].orca_compat.is_none());
        assert!(rows[0].tools.is_empty());
    }

    // ── PluginListArgs / PluginInstallArgs defaults ───────────────────────────

    #[test]
    fn list_args_default_is_first_page_no_limit() {
        let a = PluginListArgs::default();
        assert!(a.limit.is_none());
        assert!(a.cursor.is_none());
    }

    #[test]
    fn install_args_default_is_all_none_stable() {
        let a = PluginInstallArgs::default();
        assert!(a.file.is_none());
        assert!(a.name.is_none());
        assert!(a.version.is_none());
        assert!(!a.prerelease);
    }

    // ── output-struct serde (camelCase wire shapes) ───────────────────────────

    #[test]
    fn install_output_serializes_camel_case() {
        let out = PluginInstallOutput {
            software: "jellyfin".to_string(),
            version: "0.2.0".to_string(),
            tools: vec!["jellyfin.list".to_string()],
            installed_path: "/root/.orca/plugins/jellyfin".to_string(),
            loaded_live: true,
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"software\":\"jellyfin\""), "{json}");
        assert!(json.contains("\"version\":\"0.2.0\""), "{json}");
        assert!(json.contains("\"tools\":[\"jellyfin.list\"]"), "{json}");
        assert!(
            json.contains("\"installedPath\":\"/root/.orca/plugins/jellyfin\""),
            "{json}"
        );
        assert!(json.contains("\"loadedLive\":true"), "{json}");
        assert!(!json.contains("installed_path"), "{json}");
        assert!(!json.contains("loaded_live"), "{json}");
    }

    #[test]
    fn uninstall_output_serializes_camel_case() {
        let out = PluginUninstallOutput {
            software: "plex".to_string(),
            removed_from_disk: true,
            unloaded: false,
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"software\":\"plex\""), "{json}");
        assert!(json.contains("\"removedFromDisk\":true"), "{json}");
        assert!(json.contains("\"unloaded\":false"), "{json}");
        assert!(!json.contains("removed_from_disk"), "{json}");
    }

    #[test]
    fn invoke_output_serializes_tool_and_result() {
        let out = PluginInvokeOutput {
            tool: "proxmox.put_set_timezone".to_string(),
            result: serde_json::json!({"ok": true}),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("\"tool\":\"proxmox.put_set_timezone\""),
            "{json}"
        );
        assert!(json.contains("\"result\":{\"ok\":true}"), "{json}");
    }

    #[test]
    fn create_output_is_untagged_install_and_invoke() {
        // The untagged enum serializes as the inner payload with no variant tag.
        let install = PluginCreateOutput::Install(PluginInstallOutput {
            software: "s".to_string(),
            version: "1".to_string(),
            tools: Vec::new(),
            installed_path: "/p".to_string(),
            loaded_live: true,
        });
        let ijson = serde_json::to_string(&install).unwrap();
        assert!(ijson.contains("\"software\":\"s\""), "{ijson}");
        assert!(!ijson.contains("Install"), "no variant tag: {ijson}");

        let invoke = PluginCreateOutput::Invoke(PluginInvokeOutput {
            tool: "x.y".to_string(),
            result: serde_json::json!(null),
        });
        let vjson = serde_json::to_string(&invoke).unwrap();
        assert!(vjson.contains("\"tool\":\"x.y\""), "{vjson}");
        assert!(!vjson.contains("Invoke"), "no variant tag: {vjson}");
    }

    #[test]
    fn detail_output_serializes_camel_case_keys() {
        let d = PluginDetailOutput {
            name: "jellyfin".to_string(),
            catalog: None,
            installed_version: Some("0.3.0".to_string()),
            target_compat: Some(">=1.0.0".to_string()),
            orca_compat: Some(">=0.1.0".to_string()),
            tools: vec!["jellyfin.list".to_string()],
            status: PluginLoadStatus::Loaded,
            sideloaded: true,
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"installedVersion\":\"0.3.0\""), "{json}");
        assert!(json.contains("\"targetCompat\":\">=1.0.0\""), "{json}");
        assert!(json.contains("\"orcaCompat\":\">=0.1.0\""), "{json}");
        assert!(json.contains("\"status\":\"loaded\""), "{json}");
        assert!(json.contains("\"sideloaded\":true"), "{json}");
        assert!(!json.contains("installed_version"), "{json}");
    }

    #[test]
    fn list_row_serializes_tool_count_not_tools() {
        let row = PluginListRow {
            name: "jellyfin".to_string(),
            catalog: None,
            installed_version: None,
            tool_count: 3,
            status: PluginLoadStatus::NotInstalled,
            sideloaded: false,
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"toolCount\":3"), "{json}");
        assert!(json.contains("\"status\":\"notInstalled\""), "{json}");
        // The thin row must never carry the heavy tools array.
        assert!(!json.contains("\"tools\""), "{json}");
    }

    // ── serve-asset arg/output serde ──────────────────────────────────────────

    #[test]
    fn serve_asset_args_omits_absent_version() {
        let args = PluginServeAssetArgs {
            name: "sonarr".to_string(),
            repo_url: "https://github.com/argyle-labs/sonarr".to_string(),
            target: "x86_64-unknown-linux-musl".to_string(),
            version: None,
            prerelease: false,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(!json.contains("version"), "absent version omitted: {json}");
        assert!(json.contains("\"prerelease\":false"), "{json}");
    }

    #[test]
    fn serve_asset_args_includes_present_version() {
        let args = PluginServeAssetArgs {
            name: "sonarr".to_string(),
            repo_url: "https://github.com/argyle-labs/sonarr".to_string(),
            target: "aarch64-apple-darwin".to_string(),
            version: Some("0.1.1-rc.2".to_string()),
            prerelease: true,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(json.contains("\"version\":\"0.1.1-rc.2\""), "{json}");
        assert!(json.contains("\"prerelease\":true"), "{json}");
    }

    #[test]
    fn serve_asset_output_default_and_serde() {
        let def = PluginServeAssetOutput::default();
        assert!(def.asset_b64.is_empty());
        assert!(def.sha256.is_empty());
        assert!(def.version.is_empty());

        let out = PluginServeAssetOutput {
            asset_b64: "YWJj".to_string(),
            sha256: "deadbeef".to_string(),
            version: "0.2.0".to_string(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"asset_b64\":\"YWJj\""), "{json}");
        assert!(json.contains("\"sha256\":\"deadbeef\""), "{json}");
        assert!(json.contains("\"version\":\"0.2.0\""), "{json}");
    }

    // ── plugin.delete registration + role gate ────────────────────────────────

    #[test]
    fn plugin_delete_is_registered_and_admin() {
        // Uninstall is a destructive verb: it must default-deny to admin.
        assert_eq!(dispatch::required_role("plugin.delete"), Some("admin"));
    }

    // ── parse_json_object: non-object scalars are rejected ─────────────────────

    #[test]
    fn parse_json_object_rejects_scalar_json() {
        // Only a JSON object is a legal args payload; every scalar and null is
        // refused so a caller can't smuggle a bare value where a map is required.
        for bad in ["1", "1.5", "true", "false", "null"] {
            assert!(
                parse_json_object(bad).is_err(),
                "scalar {bad} must be rejected"
            );
        }
    }

    #[test]
    fn parse_json_object_accepts_nested_object() {
        // A nested object parses; the value round-trips through serialization.
        let parsed = parse_json_object(r#"{"a":{"b":[1,2]}}"#).expect("nested object parses");
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            r#"{"a":{"b":[1,2]}}"#
        );
    }

    // ── asset_label: provenance framing ────────────────────────────────────────

    #[test]
    fn asset_label_frames_name_and_triple_with_peer_marker() {
        // Empty components still produce the stable `<name>-<peer-served>-<triple>`
        // shape — the label is a pure format, never a validation gate.
        assert_eq!(asset_label("", ""), "-<peer-served>-");
        assert_eq!(
            asset_label("jellyfin", "aarch64-apple-darwin"),
            "jellyfin-<peer-served>-aarch64-apple-darwin"
        );
    }

    // ── build_plugin_list_rows: a catalog plugin loaded live but absent on disk ─

    #[test]
    fn catalog_row_loaded_live_without_on_disk_is_loaded_not_sideloaded() {
        // A catalog entry loaded live (but not scanned from disk) is still a
        // first-class catalog row — Loaded, carrying its live version, never
        // relegated to the sideloaded tail.
        let catalog = vec![entry("jellyfin", "available")];
        let live = vec![loaded("jellyfin")];
        let rows = build_plugin_list_rows(&catalog, &live, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "jellyfin");
        assert_eq!(rows[0].status, PluginLoadStatus::Loaded);
        assert!(!rows[0].sideloaded);
        assert_eq!(rows[0].installed_version.as_deref(), Some("1.2.3"));
    }

    // ── catalog cache + async tool bodies (network-free via seeded cache) ──────
    //
    // `catalog_resolved` reads a fresh in-process cache before hitting GitHub, so
    // seeding it lets us exercise `plugin.list`/`plugin.detail`/`install_from_catalog`
    // deterministically offline. All serialized under `env` since they also drive
    // ORCA_HOME and share the process-global catalog cache.

    fn seed_catalog(entries: Vec<CatalogEntry>) {
        *catalog_cache().lock().unwrap() = Some((std::time::Instant::now(), entries));
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn catalog_resolved_returns_fresh_cache_without_network() {
        seed_catalog(vec![entry("cached-only-xyz", "available")]);
        let got = catalog_resolved().await;
        assert!(
            got.iter().any(|e| e.name == "cached-only-xyz"),
            "cache-hit path must return the seeded catalog verbatim"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn plugin_list_projects_cached_catalog_to_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        seed_catalog(vec![
            entry("alpha", "available"),
            entry("beta", "unreleased"),
        ]);

        let out = plugin_list(PluginListArgs::default(), &ctx).await.unwrap();
        let names: Vec<&str> = out.plugins.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"beta"), "{names:?}");
        assert_eq!(out.total, Some(2));
        assert!(out.next_cursor.is_none());
        for row in &out.plugins {
            assert_eq!(row.status, PluginLoadStatus::NotInstalled);
            assert_eq!(row.tool_count, 0);
            assert!(!row.sideloaded);
        }
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn plugin_detail_finds_cached_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        seed_catalog(vec![entry("gamma", "planned")]);

        let d = plugin_detail(
            PluginDetailArgs {
                name: "gamma".to_string(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(d.name, "gamma");
        assert_eq!(d.status, PluginLoadStatus::NotInstalled);
        assert!(d.catalog.is_some());
        assert!(d.installed_version.is_none());
        assert!(d.tools.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn plugin_detail_errors_for_unknown_name() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        seed_catalog(vec![entry("gamma", "planned")]);

        let err = plugin_detail(
            PluginDetailArgs {
                name: "no-such-plugin-xyz".to_string(),
            },
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no such plugin"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn install_by_name_unknown_catalog_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        seed_catalog(vec![entry("known", "available")]);

        let err = plugin_install(
            PluginInstallArgs {
                file: None,
                name: Some("totally-unknown-plugin".to_string()),
                version: None,
                prerelease: false,
            },
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not in the plugin catalog"),
            "unexpected: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn install_by_name_non_available_status_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        seed_catalog(vec![entry("wip", "unreleased")]);

        let err = plugin_install(
            PluginInstallArgs {
                file: None,
                name: Some("wip".to_string()),
                version: None,
                prerelease: false,
            },
            &ctx,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not installable from the catalog yet"),
            "{msg}"
        );
        assert!(msg.contains("unreleased"), "{msg}");
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn delegate_plugin_fetch_bails_when_no_paired_peers() {
        // With a fresh temp ORCA_HOME the peer db is empty, so delegate-on-miss
        // has no secure peer (and no peers at all) to relay the fetch to. It must
        // fail with the actionable "no paired peers at all" guidance rather than
        // attempting any RemoteExec dispatch.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        let entry = entry("sonarr", "available");
        let err = match delegate_plugin_fetch(&entry, None, false, &ctx).await {
            Ok(_) => panic!("expected delegate fetch to fail with no paired peers"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no paired secure peer available")
                && msg.contains("no paired peers at all"),
            "unexpected: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial(env)]
    fn scan_and_load_ignores_non_executable_dir_contents() {
        // The install dir exists but holds only a non-executable file (a stray
        // README). `scan_and_load` must read the dir, skip the non-plugin file,
        // and return empty lists without ever attempting a spawn.
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("README.md"), b"not a plugin").unwrap();

        let (loaded, failed) = scan_and_load();
        assert!(loaded.is_empty(), "no executable plugins to load");
        assert!(
            failed.is_empty(),
            "a non-executable file is skipped, not failed"
        );
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn delegate_plugin_fetch_lists_insecure_candidates_when_none_secure() {
        // A paired but NOT-secure peer is present. Delegate-on-miss has no secure
        // candidate to relay to, so it must bail with the "Trust a candidate peer"
        // guidance that names the insecure peer — the branch distinct from the
        // no-peers-at-all message.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);

        let peer_id = utils::id::new();
        {
            let conn = db::open_default().expect("open orca.db under temp ORCA_HOME");
            db::pod::peerdb::upsert_peer(
                &conn,
                &peer_id,
                "insecure-host",
                "10.0.0.9",
                9443,
                None,
                "",
            )
            .expect("upsert insecure peer");
        }

        let entry = entry("sonarr", "available");
        let err = match delegate_plugin_fetch(&entry, None, false, &ctx).await {
            Ok(_) => panic!("expected delegate fetch to fail with no secure peer"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Trust a candidate peer") && msg.contains("insecure-host"),
            "unexpected: {msg}"
        );
        assert!(
            !msg.contains("no paired peers at all"),
            "with an insecure peer present it must not claim zero peers: {msg}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn install_by_name_matches_on_target_software_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = guard_ctx(&tmp);
        seed_catalog(vec![CatalogEntry {
            name: "friendly".to_string(),
            target_software: "svc-daemon".to_string(),
            repo_url: "https://github.com/argyle-labs/svc-daemon".to_string(),
            docs_url: "https://github.com/argyle-labs/svc-daemon#readme".to_string(),
            status: "planned".to_string(),
        }]);

        let err = plugin_install(
            PluginInstallArgs {
                file: None,
                name: Some("svc-daemon".to_string()),
                version: None,
                prerelease: false,
            },
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not installable from the catalog yet"),
            "matched on target_software but was not refused: {err:#}"
        );
    }
}
