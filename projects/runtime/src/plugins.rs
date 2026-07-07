//! Plugin tool surface — standard REST verbs (`plugin.{list, detail,
//! create, update, delete}`) per [[feedback-rest-verbs-for-tool-surfaces]].
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
    /// Stored data keys (values fetched via `plugin.detail` with `data_key`).
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
// plugin.list
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

#[orca_tool(domain = "plugin", verb = "list")]
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
// plugin.detail
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

#[orca_tool(domain = "plugin", verb = "detail")]
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
// plugin.create — install a new plugin from a manifest
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
/// resolved id already exists — use `plugin.update` to modify an
/// already-installed plugin.
#[orca_tool(domain = "plugin", verb = "create")]
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
            anyhow::bail!("plugin '{id}' already exists; use plugin.update to modify");
        }
    }
    let id = crate::install::install_plugin(&args.manifest, args.instance_id.as_deref())?;
    Ok(PluginCreateOutput { id })
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.update — modify an existing plugin (enabled, creds, data)
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
/// use `plugin.create` to install.
#[orca_tool(domain = "plugin", verb = "update")]
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
// plugin.delete — remove the plugin, a credential, or a data entry
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
#[orca_tool(domain = "plugin", verb = "delete")]
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
