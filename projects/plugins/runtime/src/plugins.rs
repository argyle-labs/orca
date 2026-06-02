//! Plugin tool surface — flat 4-tool surface (`plugin.{list, detail, update,
//! delete}`). One plugin is the primary resource; credentials and KV data
//! nest into list/detail rows and are mutated through `update`/`delete` args.
//!
//! Plugin install path: `plugin.update --manifest <path-or-url>` installs
//! a third-party plugin from a manifest. Returns the new plugin id.
//!
//! Free-form JSON is intentional for the plugin KV store — per-key shape is
//! plugin-defined.
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

// ═══════════════════════════════════════════════════════════════════════════
// plugin.list
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct PluginListArgs {
    /// Filter by tier (omit for all).
    #[cfg_attr(feature = "cli", arg(long))]
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
        let credentials = db::plugin_creds::list(&conn, &p.id)
            .unwrap_or_default()
            .into_iter()
            .map(|c| PluginCredEntry {
                key: c.key,
                synced: c.synced_at.is_some(),
                updated_at: c.updated_at,
            })
            .collect();
        let data_keys = db::plugin_data::list(&conn, &p.id)
            .map(|rows| rows.into_iter().map(|r| r.key).collect())
            .unwrap_or_default();
        plugins.push(PluginRow {
            id: p.id,
            tier: p.tier,
            enabled: p.enabled,
            credentials,
            data_keys,
        });
    }
    Ok(PluginListOutput { plugins })
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.detail
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PluginDetailArgs {
    pub id: String,
    /// Fetch the value of a specific data key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
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
    let rows = db::plugins::list(&conn)?;
    let p = rows
        .into_iter()
        .find(|p| p.id == args.id)
        .ok_or_else(|| anyhow::anyhow!("plugin '{}' not registered", args.id))?;
    let credentials = db::plugin_creds::list(&conn, &p.id)?
        .into_iter()
        .map(|c| PluginCredEntry {
            key: c.key,
            synced: c.synced_at.is_some(),
            updated_at: c.updated_at,
        })
        .collect();
    let data_keys = db::plugin_data::list(&conn, &p.id)
        .map(|rows| rows.into_iter().map(|r| r.key).collect())
        .unwrap_or_default();
    let data_value =
        if let Some(k) = args.data_key.as_deref() {
            match db::plugin_data::get(&conn, &p.id, k)? {
                Some(row) => Some(sj::from_str::<sj::Value>(&row.value).with_context(|| {
                    format!("plugin_data row for {}/{k} is not valid JSON", p.id)
                })?),
                None => None,
            }
        } else {
            None
        };
    Ok(PluginDetailOutput {
        plugin: PluginRow {
            id: p.id,
            tier: p.tier,
            enabled: p.enabled,
            credentials,
            data_keys,
        },
        data_value,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// plugin.update — install / enable-disable / cred CRUD / data set / cred sync
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginUpdateArgs {
    /// Install path. Manifest URL or local file.
    #[cfg_attr(feature = "cli", arg(long))]
    pub manifest: Option<String>,
    /// Optional instance id override when installing.
    #[cfg_attr(feature = "cli", arg(long))]
    pub instance_id: Option<String>,

    /// Existing plugin id — required for every operation other than install.
    #[cfg_attr(feature = "cli", arg(long))]
    pub id: Option<String>,
    /// Enable / disable the plugin.
    #[cfg_attr(feature = "cli", arg(long))]
    pub enabled: Option<bool>,

    /// Store a credential value. `cred_key` + `cred_value`.
    #[cfg_attr(feature = "cli", arg(long))]
    pub cred_key: Option<String>,
    #[cfg_attr(feature = "cli", arg(long))]
    pub cred_value: Option<String>,
    /// Sync stored credentials to the plugin's runtime environment.
    #[cfg_attr(feature = "cli", arg(long))]
    pub cred_sync: bool,

    /// Set a plugin data entry. `data_key` + `data_value` (JSON, REST/MCP only).
    #[cfg_attr(feature = "cli", arg(skip))]
    pub data_key: Option<String>,
    #[cfg_attr(feature = "cli", arg(skip))]
    pub data_value: Option<sj::Value>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginUpdateOutput {
    pub applied: Vec<String>,
    /// Populated when `manifest` install ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_id: Option<String>,
}

/// [MUTATES STATE] Install a plugin (`manifest`), toggle enabled (`id` +
/// `enabled`), set/sync credentials, or set data — combine in one call.
#[orca_tool(domain = "plugin", verb = "update")]
async fn plugin_update(
    args: PluginUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginUpdateOutput> {
    let mut out = PluginUpdateOutput::default();

    if let Some(manifest) = &args.manifest {
        let id = crate::install::install_plugin(manifest, args.instance_id.as_deref())?;
        out.applied.push(format!("installed:{id}"));
        out.installed_id = Some(id);
    }

    if let Some(enabled) = args.enabled {
        let id = args
            .id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("id required to toggle enabled"))?;
        let conn = db::open_default()?;
        let changed = db::plugins::set_enabled(&conn, id, enabled)?;
        out.applied.push(format!(
            "enabled:{id}:{enabled}:{}",
            if changed { "yes" } else { "noop" }
        ));
    }

    match (args.cred_key.as_deref(), args.cred_value.as_deref()) {
        (Some(k), Some(v)) => {
            let id = args
                .id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("id required to set credential"))?;
            let conn = db::open_default()?;
            db::plugin_creds::set(&conn, id, k, v)?;
            out.applied.push(format!("cred-set:{id}:{k}"));
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("cred_key and cred_value must be set together");
        }
        (None, None) => {}
    }

    if args.cred_sync {
        let id = args
            .id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("id required to sync credentials"))?;
        db::plugin_creds::sync(id)?;
        out.applied.push(format!("cred-sync:{id}"));
    }

    match (args.data_key.as_deref(), args.data_value.clone()) {
        (Some(k), Some(v)) => {
            let id = args
                .id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("id required to set data"))?;
            let conn = db::open_default()?;
            let text = sj::to_string(&v)?;
            db::plugin_data::set(&conn, id, k, &text)?;
            out.applied.push(format!("data-set:{id}:{k}"));
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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDeleteArgs {
    pub id: String,
    /// Remove a stored credential by key (leaves the plugin in place).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub cred_key: Option<String>,
    /// Remove a stored data entry by key (leaves the plugin in place).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub data_key: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginDeleteOutput {
    pub id: String,
    pub applied: Vec<String>,
}

#[orca_tool(domain = "plugin", verb = "delete")]
async fn plugin_delete(
    args: PluginDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PluginDeleteOutput> {
    let mut out = PluginDeleteOutput {
        id: args.id.clone(),
        applied: Vec::new(),
    };

    if let Some(k) = &args.cred_key {
        let conn = db::open_default()?;
        let changed = db::plugin_creds::delete(&conn, &args.id, k)?;
        out.applied.push(format!(
            "cred-removed:{k}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }
    if let Some(k) = &args.data_key {
        let conn = db::open_default()?;
        let changed = db::plugin_data::delete(&conn, &args.id, k)?;
        out.applied.push(format!(
            "data-removed:{k}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }
    if args.cred_key.is_none() && args.data_key.is_none() {
        // Whole-plugin removal.
        let changed = crate::install::remove_plugin(&args.id)?;
        out.applied.push(format!(
            "plugin-removed:{}",
            if changed { "yes" } else { "absent" }
        ));
    }
    Ok(out)
}
