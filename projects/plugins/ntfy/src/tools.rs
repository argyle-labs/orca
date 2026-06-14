//! ntfy tool surface.
//!
//! Endpoint registry: `ntfy.{list, detail, create, update, delete}` —
//! generated wholesale by `#[endpoint_resource]`. The macro emits the row
//! struct, db helpers (`endpoint_db::*`), schema fragment, args/output
//! types, and the five `#[orca_tool]`-annotated functions in one shot.
//! See [[feedback-plugin-toolkit-max-power-min-boilerplate]].
//!
//! `ntfy.send` is hand-written — it loads `(base_url, topic, token)` from
//! the generated `endpoint_db` and POSTs over HTTP.
//!
//! NOTE: changes to endpoints take effect on next daemon restart. The
//! notifications dispatcher reads the table at bootstrap.
//!
//! Imports flow through `plugin_toolkit::prelude::*` only — the
//! plugin treats the toolkit as the single gateway to the orca system.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;

use crate::{Client, Config, Message};

// ═══════════════════════════════════════════════════════════════════════════
// ntfy.{list,detail,create,update,delete} — endpoint registry CRUD.
// ═══════════════════════════════════════════════════════════════════════════

#[endpoint_resource(plugin = "ntfy")]
pub struct NtfyEndpoint {
    pub name: String,
    pub base_url: String,
    pub topic: String,
    #[secret]
    pub token: Option<String>,
    pub enabled: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// ntfy.send — raw send via a registered endpoint
// ═══════════════════════════════════════════════════════════════════════════

#[plugin_struct(args)]
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

#[plugin_struct]
#[serde(rename_all = "camelCase")]
pub struct NtfySendOutput {
    pub status: u16,
    pub ok: bool,
}

/// Send a raw ntfy message via a registered endpoint. Bypasses the routing
/// engine — use `notify.send` for normal operator notifications. Useful for
/// smoke-testing a freshly-added endpoint.
#[orca_tool(domain = "ntfy", verb = "send", role = "admin")]
async fn ntfy_send(args: NtfySendArgs, _ctx: &ToolCtx) -> Result<NtfySendOutput> {
    let conn = runtime::open_db()?;
    let row = endpoint_db::get(&conn, &args.endpoint)?
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
