use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::prelude::*;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct PluginInfo {
    pub id: String,
    pub tier: String,
    pub description: String,
    pub enabled: bool,
    #[serde(rename = "mcpCommand", skip_serializing_if = "Option::is_none")]
    pub mcp_command: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CredInfo {
    pub key: String,
    pub synced: bool,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Deserialize, ToSchema)]
pub struct SetCredRequest {
    pub key: String,
    pub value: String,
}

// ── GET /api/plugins ──────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/plugins",
    operation_id = "listPlugins",
    responses(
        (status = 200, description = "All registered plugins", body = Vec<PluginInfo>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugins_list_handler() -> Response {
    db_json(|| {
        let conn = brain_utils::db::open_default()?;
        let plugins = brain_utils::db::list_plugins(&conn)?
            .into_iter()
            .map(|p| PluginInfo {
                id: p.id,
                tier: p.tier,
                description: p.manifest_path,
                enabled: p.enabled,
                mcp_command: p.mcp_command,
            })
            .collect::<Vec<_>>();
        Ok(plugins)
    })
}

// ── GET /api/plugins/:id/creds ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/plugins/{id}/creds",
    operation_id = "listPluginCreds",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, description = "Credential keys for plugin (no values)", body = Vec<CredInfo>),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_creds_list_handler(Path(id): Path<String>) -> Response {
    db_json(|| {
        let conn = brain_utils::db::open_default()?;
        if brain_utils::db::get_plugin(&conn, &id)?.is_none() {
            anyhow::bail!("plugin '{}' not found", id);
        }
        let creds = brain_utils::db::list_plugin_credentials(&conn, &id)?
            .into_iter()
            .map(|c| CredInfo {
                key: c.key,
                synced: c.synced_at.is_some(),
                updated_at: c.updated_at,
            })
            .collect::<Vec<_>>();
        Ok(creds)
    })
}

// ── PUT /api/plugins/:id/creds ────────────────────────────────────────────────

#[utoipa::path(
    put,
    path = "/api/plugins/{id}/creds",
    operation_id = "setPluginCred",
    params(("id" = String, Path, description = "Plugin ID")),
    request_body = SetCredRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_creds_set_handler(
    Path(id): Path<String>,
    Json(body): Json<SetCredRequest>,
) -> Response {
    db_ok(|| {
        let conn = brain_utils::db::open_default()?;
        if brain_utils::db::get_plugin(&conn, &id)?.is_none() {
            anyhow::bail!("plugin '{}' not found", id);
        }
        brain_utils::db::set_plugin_credential(&conn, &id, &body.key, &body.value)?;
        Ok(())
    })
}

// ── DELETE /api/plugins/:id/creds/:key ───────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/plugins/{id}/creds/{key}",
    operation_id = "deletePluginCred",
    params(
        ("id" = String, Path, description = "Plugin ID"),
        ("key" = String, Path, description = "Credential key"),
    ),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_creds_delete_handler(Path((id, key)): Path<(String, String)>) -> Response {
    db_remove("credential", &key, || {
        let conn = brain_utils::db::open_default()?;
        brain_utils::db::delete_plugin_credential(&conn, &id, &key)
    })
}

// ── POST /api/plugins/:id/creds/sync ─────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/plugins/{id}/creds/sync",
    operation_id = "syncPluginCreds",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, description = "Number of credentials synced", body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_creds_sync_handler(Path(id): Path<String>) -> Response {
    match brain_commands::creds_cmd::sync_plugin_creds(&id) {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
