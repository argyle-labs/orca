//! ntfy endpoint CRUD tools (`ntfy.add`, `ntfy.list`, `ntfy.delete`, `ntfy.send`).
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

// ── ntfy.add ───────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NtfyAddArgs {
    /// Short name — referenced by routing rules (`send = [\"<name>\"]`).
    #[arg(long)]
    pub name: String,
    /// ntfy base URL, e.g. `http://10.10.10.6:8080` or `https://ntfy.sh`.
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
pub struct NtfyAddOutput {
    pub endpoint: NtfyEndpointEntry,
}

/// Register (or update) an ntfy endpoint. Upsert on `name`. The new endpoint
/// is also registered live with the notifications dispatcher — no restart
/// required.
#[orca_tool(domain = "ntfy", verb = "add", role = "admin")]
async fn ntfy_add(args: NtfyAddArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<NtfyAddOutput> {
    let row = db::ntfy::EndpointRow {
        name: args.name.clone(),
        base_url: args.base_url,
        topic: args.topic,
        token: args.token,
        enabled: args.enabled.unwrap_or(true),
    };
    let conn = db::open_default()?;
    db::ntfy::upsert(&conn, &row)?;
    if row.enabled {
        crate::register_endpoint(&row);
    }
    Ok(NtfyAddOutput {
        endpoint: to_entry(row),
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

/// Delete an ntfy endpoint row. Note: the live backend remains registered in
/// the running dispatcher until the daemon restarts (the dispatcher exposes
/// no unregister API yet — tracked separately).
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
