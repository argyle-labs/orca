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

use std::path::{Path, PathBuf};

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
            match plugin_loader::spawn_plugin(&path) {
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

/// One row in `plugin.list`: a catalog and/or installed/loaded plugin, joined
/// on the `target_software` name.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginListRow {
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
    /// Tool names this plugin contributes, when loaded.
    pub tools: Vec<String>,
    /// Whether the plugin is a known first-party catalog entry, sideloaded, or
    /// merely planned.
    pub status: PluginLoadStatus,
    /// True when this plugin is not in the catalog (a sideloaded third party).
    pub sideloaded: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginListOutput {
    /// One row per known catalog plugin, plus a row for any loaded/installed
    /// plugin not in the catalog (sideloaded third parties).
    pub plugins: Vec<PluginListRow>,
}

/// List the first-party catalog joined with installed + loaded plugins. The UI
/// and CLI use this to show the known roster, what is live, and where to read
/// about each.
#[orca_tool(domain = "plugin", verb = "list")]
async fn plugin_list(_args: PluginListArgs, _ctx: &ToolCtx) -> Result<PluginListOutput> {
    let catalog = catalog_resolved().await;
    let loaded = plugin_loader::loaded_plugins();
    let installed_on_disk = installed_software_on_disk();
    let rows = build_plugin_list_rows(&catalog, &loaded, &installed_on_disk);
    Ok(PluginListOutput { plugins: rows })
}

/// Pure join/dedup behind `plugin.list`: catalog rows first (in catalog order,
/// joined to the live/on-disk state), then any loaded/installed plugin not in
/// the catalog as a sorted, deduped sideloaded tail. Split out from the tool
/// body so the row-building logic is testable without the live registry / disk.
fn build_plugin_list_rows(
    catalog: &[CatalogEntry],
    loaded: &[plugin_loader::LoadedPluginInfo],
    installed_on_disk: &[String],
) -> Vec<PluginListRow> {
    let mut rows: Vec<PluginListRow> = Vec::new();

    // Catalog rows first, in catalog order.
    for entry in catalog {
        let live = loaded.iter().find(|l| l.software == entry.target_software);
        let on_disk = installed_on_disk.contains(&entry.target_software);
        let status = match (live.is_some(), on_disk) {
            (true, _) => PluginLoadStatus::Loaded,
            (false, true) => PluginLoadStatus::InstalledNotLoaded,
            (false, false) => PluginLoadStatus::NotInstalled,
        };
        rows.push(PluginListRow {
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
        rows.push(PluginListRow {
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
#[orca_tool(domain = "plugin", verb = "install")]
async fn plugin_install(args: PluginInstallArgs, _ctx: &ToolCtx) -> Result<PluginInstallOutput> {
    if args.file.is_some() && args.name.is_some() {
        bail!("pass exactly one of --file (sideload) or --name (catalog install), not both");
    }

    if let Some(name) = &args.name {
        return install_from_catalog(name, args.version.as_deref(), args.prerelease).await;
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
        let report = plugin_loader::spawn_plugin(src)
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
            std::fs::copy(src, &dest)
                .with_context(|| format!("failed to copy plugin into {}", dest.display()))?;
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

    let fetched =
        crate::plugin_fetch::fetch(&entry.target_software, &entry.repo_url, version, prerelease)
            .await?;

    let dir = install_dir().context("cannot resolve plugin install dir (no orca_home)")?;
    files::ops::mkdir_p(&dir)?;
    let dest = dir.join(install_filename(&entry.target_software));
    std::fs::write(&dest, &fetched.bytes)
        .with_context(|| format!("failed to write plugin to {}", dest.display()))?;

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
        let report = match plugin_loader::spawn_plugin(&dest) {
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

// ── plugin.uninstall ─────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginUninstallArgs {
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
#[orca_tool(domain = "plugin", verb = "uninstall")]
async fn plugin_uninstall(
    args: PluginUninstallArgs,
    _ctx: &ToolCtx,
) -> Result<PluginUninstallOutput> {
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
}
