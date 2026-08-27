//! Plugin DATA surface — the DB-backed plugin registry + per-plugin KV store,
//! exposed as standard REST verbs (`plugin.data.{list, detail, create, update,
//! delete}`) per [[feedback-rest-verbs-for-tool-surfaces]]. Distinct from the
//! `plugin.*` MANAGEMENT surface (catalog/install/load/invoke) in
//! `system::plugin_manager` — this owns the stored plugin records + their data.
//!
//! - `create` installs a plugin from a manifest; errors if the id already
//!   exists.
//! - `update` modifies an existing plugin's enabled flag, credentials, or
//!   data; errors if the id is unknown. Never installs.
//! - `delete` removes the plugin, or a single credential/data key.
//!
//! Credentials and data keys are nested sub-resources mutated through
//! `update` / `delete` arg combinations rather than separate tool
//! surfaces (the surface is small enough that splitting buys nothing).
//!
//! Free-form JSON is intentional for the plugin KV store — per-key shape
//! is plugin-defined.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json as sj;

use derive::orca_tool;

// ── Row shapes ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginCredEntry {
    pub key: String,
    /// `true` once the credential has been synced to the plugin runtime.
    pub synced: bool,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginRow {
    pub id: String,
    pub tier: String,
    pub enabled: bool,
    /// Stored credential keys (values never returned).
    pub credentials: Vec<PluginCredEntry>,
    /// Stored data keys (values fetched via `plugin.data.detail` with `data_key`).
    pub data_keys: Vec<String>,
}

fn load_row(conn: &rusqlite::Connection, id: &str) -> anyhow::Result<PluginRow> {
    let p = db::plugins::list(conn)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| anyhow::anyhow!("plugin '{id}' not registered"))?;
    let credentials = db::plugin_creds::list(conn, &p.id)
        .unwrap_or_default()
        .into_iter()
        .map(|c| PluginCredEntry {
            key: c.key,
            synced: c.synced_at.is_some(),
            updated_at: c.updated_at,
        })
        .collect();
    let data_keys = db::plugin_data::list(conn, &p.id)
        .map(|rows| rows.into_iter().map(|r| r.key).collect())
        .unwrap_or_default();
    Ok(PluginRow {
        id: p.id,
        tier: p.tier,
        enabled: p.enabled,
        credentials,
        data_keys,
    })
}

fn plugin_exists(conn: &rusqlite::Connection, id: &str) -> anyhow::Result<bool> {
    Ok(db::plugins::list(conn)?.into_iter().any(|p| p.id == id))
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.data.list
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct PluginListArgs {
    /// Filter by tier (omit for all).
    #[arg(long)]
    pub tier: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginListOutput {
    pub plugins: Vec<PluginRow>,
}

#[orca_tool(domain = "plugin.data", verb = "list")]
async fn plugin_list(
    args: PluginListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginListOutput> {
    let conn = db::open_default()?;
    let rows = db::plugins::list(&conn)?;
    let mut plugins = Vec::with_capacity(rows.len());
    for p in rows {
        if let Some(t) = args.tier.as_deref()
            && p.tier != t
        {
            continue;
        }
        plugins.push(load_row(&conn, &p.id)?);
    }
    Ok(PluginListOutput { plugins })
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.data.detail
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PluginDetailArgs {
    pub id: String,
    /// Fetch the value of a specific data key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub data_key: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDetailOutput {
    pub plugin: PluginRow,
    /// Populated when `data_key` was supplied — the JSON value at that key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_value: Option<sj::Value>,
}

#[orca_tool(domain = "plugin.data", verb = "detail")]
async fn plugin_detail(
    args: PluginDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginDetailOutput> {
    use anyhow::Context;
    let conn = db::open_default()?;
    let plugin = load_row(&conn, &args.id)?;
    let data_value = if let Some(k) = args.data_key.as_deref() {
        match db::plugin_data::get(&conn, &plugin.id, k)? {
            Some(row) => Some(sj::from_str::<sj::Value>(&row.value).with_context(|| {
                format!("plugin_data row for {}/{k} is not valid JSON", plugin.id)
            })?),
            None => None,
        }
    } else {
        None
    };
    Ok(PluginDetailOutput { plugin, data_value })
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.data.create — install a new plugin from a manifest
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginCreateArgs {
    /// Manifest URL or local file describing the plugin to install.
    #[arg(long)]
    pub manifest: String,
    /// Optional instance id override (defaults to the manifest's id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub instance_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginCreateOutput {
    pub id: String,
}

/// [MUTATES STATE] Install a plugin from a manifest. Errors if the
/// resolved id already exists — use `plugin.data.update` to modify an
/// already-installed plugin.
#[orca_tool(domain = "plugin.data", verb = "create")]
async fn plugin_create(
    args: PluginCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginCreateOutput> {
    // Pre-check: if the caller specified an instance_id, refuse early on
    // collision. (When the id is derived from the manifest, the conflict
    // surfaces inside `install_plugin`; we still wrap that with a clean
    // error below.)
    if let Some(id) = args.instance_id.as_deref() {
        let conn = db::open_default()?;
        if plugin_exists(&conn, id)? {
            anyhow::bail!("plugin '{id}' already exists; use plugin.data.update to modify");
        }
    }
    let id = crate::install::install_plugin(&args.manifest, args.instance_id.as_deref())?;
    Ok(PluginCreateOutput { id })
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.data.update — modify an existing plugin (enabled, creds, data)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateArgs {
    /// Plugin id to modify.
    pub id: String,
    /// Enable / disable the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub enabled: Option<bool>,

    /// Store a credential value. `cred_key` + `cred_value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub cred_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub cred_value: Option<String>,
    /// Sync stored credentials to the plugin's runtime environment.
    #[serde(default)]
    #[arg(long)]
    pub cred_sync: bool,

    /// Set a plugin data entry. `data_key` + `data_value` (JSON, REST/MCP only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(skip)]
    pub data_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(skip)]
    pub data_value: Option<sj::Value>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginUpdateOutput {
    pub id: String,
    pub applied: Vec<String>,
}

/// [MUTATES STATE] Modify an existing plugin: toggle enabled, set/sync
/// credentials, set data. Errors if `id` is not a registered plugin —
/// use `plugin.data.create` to install.
#[orca_tool(domain = "plugin.data", verb = "update")]
async fn plugin_update(
    args: PluginUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginUpdateOutput> {
    let conn = db::open_default()?;
    if !plugin_exists(&conn, &args.id)? {
        anyhow::bail!(
            "plugin '{}' not registered; use plugin.create to install",
            args.id
        );
    }

    let mut out = PluginUpdateOutput {
        id: args.id.clone(),
        applied: Vec::new(),
    };

    if let Some(enabled) = args.enabled {
        let changed = db::plugins::set_enabled(&conn, &args.id, enabled)?;
        out.applied.push(format!(
            "enabled:{}:{}",
            enabled,
            if changed { "yes" } else { "noop" }
        ));
    }

    match (args.cred_key.as_deref(), args.cred_value.as_deref()) {
        (Some(k), Some(v)) => {
            db::plugin_creds::set(&conn, &args.id, k, v)?;
            out.applied.push(format!("cred-set:{k}"));
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("cred_key and cred_value must be set together");
        }
        (None, None) => {}
    }

    if args.cred_sync {
        db::plugin_creds::sync(&args.id)?;
        out.applied.push("cred-sync".to_string());
    }

    match (args.data_key.as_deref(), args.data_value.clone()) {
        (Some(k), Some(v)) => {
            let text = sj::to_string(&v)?;
            db::plugin_data::set(&conn, &args.id, k, &text)?;
            out.applied.push(format!("data-set:{k}"));
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("data_key and data_value must be set together");
        }
        (None, None) => {}
    }

    if out.applied.is_empty() {
        anyhow::bail!("no plugin.update operation specified");
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.data.delete — remove the plugin, a credential, or a data entry
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDeleteArgs {
    pub id: String,
    /// Remove a stored credential by key (leaves the plugin in place).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub cred_key: Option<String>,
    /// Remove a stored data entry by key (leaves the plugin in place).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub data_key: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginDeleteOutput {
    pub id: String,
    pub applied: Vec<String>,
}

/// [MUTATES STATE] Delete the whole plugin, or just a single credential
/// or data entry. Errors if `id` is not registered.
#[orca_tool(domain = "plugin.data", verb = "delete")]
async fn plugin_delete(
    args: PluginDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginDeleteOutput> {
    let conn = db::open_default()?;
    if !plugin_exists(&conn, &args.id)? {
        anyhow::bail!("plugin '{}' not registered", args.id);
    }

    let mut out = PluginDeleteOutput {
        id: args.id.clone(),
        applied: Vec::new(),
    };

    if let Some(k) = &args.cred_key {
        let changed = db::plugin_creds::delete(&conn, &args.id, k)?;
        out.applied.push(format!(
            "cred-removed:{k}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }
    if let Some(k) = &args.data_key {
        let changed = db::plugin_data::delete(&conn, &args.id, k)?;
        out.applied.push(format!(
            "data-removed:{k}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }
    if args.cred_key.is_none() && args.data_key.is_none() {
        let changed = crate::install::remove_plugin(&args.id)?;
        out.applied.push(format!(
            "plugin-removed:{}",
            if changed { "yes" } else { "absent" }
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::config::{Config, Model, Ports};
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    /// Minimal single-thread executor: the tool bodies exercised here never
    /// register a real waker (every await resolves to a synchronous DB call,
    /// so the future is `Ready` on the first poll), letting us drive them
    /// without pulling a tokio runtime into this crate's dev-deps.
    fn block_on<F: Future>(fut: F) -> F::Output {
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }

    /// A throwaway `ToolCtx`. Every tool body in this file ignores `_ctx`, so
    /// the config contents are irrelevant — only its existence matters.
    fn test_ctx() -> contract::ToolCtx {
        contract::ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Ports::default(),
        }))
    }

    /// Open a fresh unencrypted DB inside `dir` (schema auto-created on open).
    fn open_db(dir: &std::path::Path) -> rusqlite::Connection {
        db::open_unencrypted(&dir.join("plugins-test.db")).unwrap()
    }

    /// Seed a bare plugin row so the registry-existence checks pass.
    fn seed_plugin(conn: &rusqlite::Connection, id: &str, tier: &str) {
        db::plugins::upsert(
            conn,
            &db::plugins::PluginRow {
                id: id.into(),
                manifest_path: format!("/tmp/{id}.toml"),
                tier: tier.into(),
                context_injection: "minimal".into(),
                enabled: true,
                command_map: Default::default(),
                nav_links: Vec::new(),
                search_tools: Vec::new(),
                specs_dir: None,
            },
        )
        .unwrap();
    }

    // ── plugin_exists / load_row ────────────────────────────────────────────

    #[test]
    fn plugin_exists_reflects_registry() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path());
        assert!(!plugin_exists(&conn, "ghost").unwrap());
        seed_plugin(&conn, "real", "personal");
        assert!(plugin_exists(&conn, "real").unwrap());
        assert!(!plugin_exists(&conn, "other").unwrap());
    }

    #[test]
    fn load_row_errors_on_unknown_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path());
        let err = load_row(&conn, "nope").err().unwrap().to_string();
        assert!(err.contains("not registered"), "got: {err}");
    }

    #[test]
    fn load_row_projects_creds_and_data_keys() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path());
        seed_plugin(&conn, "p1", "team");
        db::plugin_creds::set(&conn, "p1", "API_KEY", "secret").unwrap();
        db::plugin_data::set(&conn, "p1", "cfg", "{\"a\":1}").unwrap();

        let row = load_row(&conn, "p1").unwrap();
        assert_eq!(row.id, "p1");
        assert_eq!(row.tier, "team");
        assert!(row.enabled);
        assert_eq!(row.credentials.len(), 1);
        assert_eq!(row.credentials[0].key, "API_KEY");
        // Credential was never synced to the runtime.
        assert!(!row.credentials[0].synced);
        assert_eq!(row.data_keys, vec!["cfg".to_string()]);
    }

    // ── plugin_list ─────────────────────────────────────────────────────────

    #[test]
    fn list_filters_by_tier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "team-a", "team");
            seed_plugin(&conn, "pers-a", "personal");
        }
        let out = block_on(db::with_db_path(path, async {
            plugin_list(
                PluginListArgs {
                    tier: Some("team".into()),
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert_eq!(out.plugins.len(), 1);
        assert_eq!(out.plugins[0].id, "team-a");
    }

    // ── plugin_detail ───────────────────────────────────────────────────────

    #[test]
    fn detail_returns_data_value_when_key_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "d1", "personal");
            db::plugin_data::set(&conn, "d1", "k", "{\"n\":42}").unwrap();
        }
        let out = block_on(db::with_db_path(path, async {
            plugin_detail(
                PluginDetailArgs {
                    id: "d1".into(),
                    data_key: Some("k".into()),
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert_eq!(out.plugin.id, "d1");
        assert_eq!(out.data_value.unwrap()["n"], sj::json!(42));
    }

    #[test]
    fn detail_missing_key_yields_none_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "d2", "personal");
        }
        let out = block_on(db::with_db_path(path, async {
            plugin_detail(
                PluginDetailArgs {
                    id: "d2".into(),
                    data_key: Some("absent".into()),
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert!(out.data_value.is_none());
    }

    // ── plugin_create ───────────────────────────────────────────────────────

    #[test]
    fn create_rejects_existing_instance_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "dup", "personal");
        }
        let err = block_on(db::with_db_path(path, async {
            plugin_create(
                PluginCreateArgs {
                    manifest: "/does/not/matter.toml".into(),
                    instance_id: Some("dup".into()),
                },
                &test_ctx(),
            )
            .await
            .err()
            .unwrap()
            .to_string()
        }));
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn create_installs_from_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        db::open_unencrypted(&path).unwrap(); // materialize schema
        let manifest = dir.path().join("orca-plugin.toml");
        std::fs::write(
            &manifest,
            "[plugin]\nid = \"fresh\"\nversion = \"1.0.0\"\ntier = \"personal\"\n",
        )
        .unwrap();
        let manifest_str = manifest.to_string_lossy().into_owned();
        let out = block_on(db::with_db_path(path, async {
            plugin_create(
                PluginCreateArgs {
                    manifest: manifest_str,
                    instance_id: None,
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert_eq!(out.id, "fresh");
    }

    // ── plugin_update ───────────────────────────────────────────────────────

    #[test]
    fn update_unknown_plugin_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        db::open_unencrypted(&path).unwrap();
        let err = block_on(db::with_db_path(path, async {
            plugin_update(
                PluginUpdateArgs {
                    id: "ghost".into(),
                    enabled: Some(true),
                    cred_key: None,
                    cred_value: None,
                    cred_sync: false,
                    data_key: None,
                    data_value: None,
                },
                &test_ctx(),
            )
            .await
            .err()
            .unwrap()
            .to_string()
        }));
        assert!(err.contains("not registered"), "got: {err}");
    }

    #[test]
    fn update_toggles_enabled_and_sets_cred_and_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "u1", "personal");
        }
        let out = block_on(db::with_db_path(path.clone(), async {
            plugin_update(
                PluginUpdateArgs {
                    id: "u1".into(),
                    enabled: Some(false),
                    cred_key: Some("TOKEN".into()),
                    cred_value: Some("v".into()),
                    cred_sync: false,
                    data_key: Some("dk".into()),
                    data_value: Some(sj::json!({"x": true})),
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert_eq!(out.id, "u1");
        assert!(out.applied.iter().any(|a| a == "enabled:false:yes"));
        assert!(out.applied.iter().any(|a| a == "cred-set:TOKEN"));
        assert!(out.applied.iter().any(|a| a == "data-set:dk"));

        // Side effects landed in the DB.
        let conn = db::open_unencrypted(&path).unwrap();
        assert!(!db::plugins::get(&conn, "u1").unwrap().unwrap().enabled);
        assert!(db::plugin_data::get(&conn, "u1", "dk").unwrap().is_some());
    }

    #[test]
    fn update_rejects_half_specified_cred() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "u2", "personal");
        }
        let err = block_on(db::with_db_path(path, async {
            plugin_update(
                PluginUpdateArgs {
                    id: "u2".into(),
                    enabled: None,
                    cred_key: Some("K".into()),
                    cred_value: None,
                    cred_sync: false,
                    data_key: None,
                    data_value: None,
                },
                &test_ctx(),
            )
            .await
            .err()
            .unwrap()
            .to_string()
        }));
        assert!(err.contains("cred_key and cred_value"), "got: {err}");
    }

    #[test]
    fn update_with_no_ops_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "u3", "personal");
        }
        let err = block_on(db::with_db_path(path, async {
            plugin_update(
                PluginUpdateArgs {
                    id: "u3".into(),
                    enabled: None,
                    cred_key: None,
                    cred_value: None,
                    cred_sync: false,
                    data_key: None,
                    data_value: None,
                },
                &test_ctx(),
            )
            .await
            .err()
            .unwrap()
            .to_string()
        }));
        assert!(err.contains("no plugin.update operation"), "got: {err}");
    }

    // ── plugin_delete ───────────────────────────────────────────────────────

    #[test]
    fn delete_unknown_plugin_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        db::open_unencrypted(&path).unwrap();
        let err = block_on(db::with_db_path(path, async {
            plugin_delete(
                PluginDeleteArgs {
                    id: "ghost".into(),
                    cred_key: None,
                    data_key: None,
                },
                &test_ctx(),
            )
            .await
            .err()
            .unwrap()
            .to_string()
        }));
        assert!(err.contains("not registered"), "got: {err}");
    }

    #[test]
    fn delete_removes_cred_and_data_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "del1", "personal");
            db::plugin_creds::set(&conn, "del1", "CK", "v").unwrap();
            db::plugin_data::set(&conn, "del1", "DK", "{}").unwrap();
        }
        let out = block_on(db::with_db_path(path, async {
            plugin_delete(
                PluginDeleteArgs {
                    id: "del1".into(),
                    cred_key: Some("CK".into()),
                    data_key: Some("DK".into()),
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert!(out.applied.iter().any(|a| a == "cred-removed:CK:yes"));
        assert!(out.applied.iter().any(|a| a == "data-removed:DK:yes"));
        // The plugin itself was left in place (only sub-resources removed).
        assert!(!out.applied.iter().any(|a| a.starts_with("plugin-removed")));
    }

    #[test]
    fn delete_absent_cred_reports_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "del2", "personal");
        }
        let out = block_on(db::with_db_path(path, async {
            plugin_delete(
                PluginDeleteArgs {
                    id: "del2".into(),
                    cred_key: Some("missing".into()),
                    data_key: None,
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert!(
            out.applied
                .iter()
                .any(|a| a == "cred-removed:missing:absent")
        );
    }

    #[test]
    fn delete_whole_plugin_when_no_subkey() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins-test.db");
        {
            let conn = db::open_unencrypted(&path).unwrap();
            seed_plugin(&conn, "del3", "personal");
        }
        let out = block_on(db::with_db_path(path.clone(), async {
            plugin_delete(
                PluginDeleteArgs {
                    id: "del3".into(),
                    cred_key: None,
                    data_key: None,
                },
                &test_ctx(),
            )
            .await
            .unwrap()
        }));
        assert!(out.applied.iter().any(|a| a == "plugin-removed:yes"));
        let conn = db::open_unencrypted(&path).unwrap();
        assert!(db::plugins::get(&conn, "del3").unwrap().is_none());
    }
}
