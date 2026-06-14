//! ntfy endpoint CRUD tools (`ntfy.{list, detail, create, update, delete, send}`).
//! Endpoints are stored in `db::ntfy::ntfy_endpoints`; on daemon start
//! [`crate::bootstrap`] registers each enabled row as a notification backend
//! with the `notifications` dispatcher.

use anyhow::Context;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

use crate::{Client, Config, Message};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfyEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub topic: String,
    pub has_token: bool,
    pub enabled: bool,
}

fn to_entry(r: db::ntfy::EndpointRow) -> NtfyEndpointEntry {
    NtfyEndpointEntry {
        name: r.name,
        base_url: r.base_url,
        topic: r.topic,
        has_token: r.token.is_some(),
        enabled: r.enabled,
    }
}

// ── ntfy.list ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct NtfyListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct NtfyListOutput {
    pub endpoints: Vec<NtfyEndpointEntry>,
}

/// List registered ntfy endpoints. Each row drives one backend in the
/// notifications dispatcher.
#[orca_tool(domain = "ntfy", verb = "list")]
async fn ntfy_list(
    _args: NtfyListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NtfyListOutput> {
    let conn = db::open_default()?;
    Ok(NtfyListOutput {
        endpoints: db::ntfy::list(&conn)?.into_iter().map(to_entry).collect(),
    })
}

// ── ntfy.detail ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct NtfyDetailArgs {
    #[arg(long)]
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfyDetailOutput {
    pub endpoint: NtfyEndpointEntry,
}

/// Detail for a single registered ntfy endpoint.
#[orca_tool(domain = "ntfy", verb = "detail")]
async fn ntfy_detail(
    args: NtfyDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NtfyDetailOutput> {
    let conn = db::open_default()?;
    let row = db::ntfy::get(&conn, &args.name)?
        .with_context(|| format!("ntfy endpoint '{}' not registered", args.name))?;
    Ok(NtfyDetailOutput {
        endpoint: to_entry(row),
    })
}

// ── ntfy.create ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfyCreateArgs {
    /// Short name — referenced by routing rules.
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub base_url: String,
    /// Topic name on the ntfy server.
    #[arg(long)]
    pub topic: String,
    /// Optional bearer token for authenticated topics.
    #[arg(long)]
    pub token: Option<String>,
    /// Defaults to true. Disabled endpoints are skipped at bootstrap.
    #[arg(long)]
    pub enabled: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfyCreateOutput {
    pub endpoint: NtfyEndpointEntry,
}

/// [MUTATES STATE] Register a new ntfy endpoint. Errors if `name` is already
/// taken — use ntfy.update to modify an existing endpoint.
#[orca_tool(domain = "ntfy", verb = "create", role = "admin")]
async fn ntfy_create(
    args: NtfyCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NtfyCreateOutput> {
    let row = db::ntfy::EndpointRow {
        name: args.name.clone(),
        base_url: args.base_url,
        topic: args.topic,
        token: args.token,
        enabled: args.enabled.unwrap_or(true),
    };
    let conn = db::open_default()?;
    db::ntfy::insert(&conn, &row).map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            anyhow::anyhow!(
                "ntfy endpoint '{}' already exists — use ntfy.update to modify it",
                row.name
            )
        } else {
            e
        }
    })?;
    if row.enabled {
        crate::register_endpoint(&row);
    }
    Ok(NtfyCreateOutput {
        endpoint: to_entry(row),
    })
}

// ── ntfy.update ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct NtfyUpdateArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub topic: Option<String>,
    #[arg(long)]
    pub token: Option<String>,
    #[arg(long)]
    pub enabled: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfyUpdateOutput {
    pub endpoint: NtfyEndpointEntry,
    pub applied: Vec<String>,
}

/// [MUTATES STATE] Modify an existing ntfy endpoint. PATCH semantics — endpoint
/// must already exist.
#[orca_tool(domain = "ntfy", verb = "update", role = "admin")]
async fn ntfy_update(
    args: NtfyUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NtfyUpdateOutput> {
    let conn = db::open_default()?;
    let mut row = db::ntfy::get(&conn, &args.name)?
        .with_context(|| format!("ntfy endpoint '{}' not registered", args.name))?;
    let mut applied = Vec::new();
    if let Some(v) = args.base_url {
        row.base_url = v;
        applied.push("base_url".into());
    }
    if let Some(v) = args.topic {
        row.topic = v;
        applied.push("topic".into());
    }
    if let Some(v) = args.token {
        row.token = Some(v);
        applied.push("token".into());
    }
    if let Some(v) = args.enabled {
        row.enabled = v;
        applied.push("enabled".into());
    }
    if applied.is_empty() {
        anyhow::bail!(
            "no fields to update; pass at least one of --base-url, --topic, --token, --enabled"
        );
    }
    let changed = db::ntfy::update(&conn, &row)?;
    if !changed {
        anyhow::bail!("update reported no row change for '{}'", row.name);
    }
    Ok(NtfyUpdateOutput {
        endpoint: to_entry(row),
        applied,
    })
}

// ── ntfy.delete ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct NtfyDeleteArgs {
    #[arg(long)]
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfyDeleteOutput {
    pub deleted: bool,
}

/// [MUTATES STATE] Remove a registered ntfy endpoint. Idempotent — returns
/// `deleted: false` if no row matched.
#[orca_tool(domain = "ntfy", verb = "delete", role = "admin")]
async fn ntfy_delete(
    args: NtfyDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NtfyDeleteOutput> {
    let conn = db::open_default()?;
    Ok(NtfyDeleteOutput {
        deleted: db::ntfy::remove(&conn, &args.name)?,
    })
}

// ── ntfy.send ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfySendArgs {
    /// Registered endpoint name (see `ntfy.list`).
    #[arg(long)]
    pub endpoint: String,
    /// Message body.
    #[arg(long)]
    pub message: String,
    /// Optional title.
    #[arg(long)]
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfySendOutput {
    pub status: u16,
    pub ok: bool,
}

/// Send a raw ntfy message via a registered endpoint. Bypasses the routing
/// engine — use `notify.send` for normal operator notifications. Useful for
/// smoke-testing a freshly-added endpoint.
#[orca_tool(domain = "ntfy", verb = "send", role = "admin")]
async fn ntfy_send(args: NtfySendArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<NtfySendOutput> {
    let conn = db::open_default()?;
    let row = db::ntfy::get(&conn, &args.endpoint)?
        .with_context(|| format!("ntfy endpoint '{}' not registered", args.endpoint))?;
    let mut cfg = Config::new(row.base_url, row.topic);
    if let Some(t) = row.token {
        cfg = cfg.with_token(t);
    }
    let client = Client::new(cfg);
    let r = client
        .send(Message {
            message: &args.message,
            title: args.title.as_deref(),
            ..Default::default()
        })
        .await?;
    Ok(NtfySendOutput {
        status: r.status,
        ok: r.ok,
    })
}
