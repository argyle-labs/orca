use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::prelude::*;

#[derive(Deserialize, ToSchema)]
pub struct PluginInstallRequest {
    /// Absolute or ~/ path to the orca-plugin.toml on the server's filesystem.
    pub manifest: String,
    /// Optional instance id override — allows multiple instances of the same
    /// plugin template with separate credentials (e.g. "atlassian@infra").
    #[serde(rename = "instanceId")]
    pub instance_id: Option<String>,
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct PluginInfo {
    pub id: String,
    pub tier: String,
    pub description: String,
    pub mode: String,
    pub enabled: bool,
    #[serde(rename = "mcpCommand", skip_serializing_if = "Option::is_none")]
    pub mcp_command: Option<String>,
    #[serde(rename = "navLinks")]
    pub nav_links: Vec<serde_json::Value>,
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
        let conn = db::open_default()?;
        let plugins = db::plugins::list(&conn)?
            .into_iter()
            .map(|p| PluginInfo {
                id: p.id,
                tier: p.tier,
                description: p.manifest_path,
                mode: p.mode,
                enabled: p.enabled,
                mcp_command: p.mcp_command,
                nav_links: p.nav_links,
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
        let conn = db::open_default()?;
        if db::plugins::get(&conn, &id)?.is_none() {
            anyhow::bail!("plugin '{}' not found", id);
        }
        let creds = db::plugin_creds::list(&conn, &id)?
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
        let conn = db::open_default()?;
        if db::plugins::get(&conn, &id)?.is_none() {
            anyhow::bail!("plugin '{}' not found", id);
        }
        db::plugin_creds::set(&conn, &id, &body.key, &body.value)?;
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
        let conn = db::open_default()?;
        db::plugin_creds::delete(&conn, &id, &key)
    })
}

// ── GET /api/plugins/:id/health ──────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/plugins/{id}/health",
    operation_id = "getPluginHealth",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, description = "Plugin health status"),
        (status = 404, body = ErrorResponse),
        (status = 502, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_health_handler(Path(id): Path<String>) -> Response {
    let mcp_command = {
        let Ok(conn) = db::open_default() else {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "db error");
        };
        let Ok(Some(plugin)) = db::plugins::get(&conn, &id) else {
            return err(StatusCode::NOT_FOUND, "plugin not found");
        };
        plugin.mcp_command.filter(|u| u.starts_with("http"))
    };

    let Some(base_url) = mcp_command else {
        return err(StatusCode::BAD_REQUEST, "plugin has no HTTP transport URL");
    };

    let token = db::open_default().ok().and_then(|conn| {
        db::plugin_creds::list(&conn, &id).ok().and_then(|creds| {
            creds
                .into_iter()
                .find(|c| c.key == "MEERKAT_TOKEN")
                .map(|c| c.value)
        })
    });

    let health_url = format!("{}/health", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let mut req = client.get(&health_url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => Json(body).into_response(),
            Err(_) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        },
        Ok(resp) => err(
            StatusCode::BAD_GATEWAY,
            &format!("plugin returned HTTP {}", resp.status()),
        ),
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("unreachable: {e}")),
    }
}

// ── GET /api/plugins/:id/data ─────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/plugins/{id}/data",
    operation_id = "listPluginData",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, description = "All data entries for plugin", body = Vec<PluginDataEntry>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_data_list_handler(Path(id): Path<String>) -> Response {
    db_json(|| {
        let conn = db::open_default()?;
        let entries = db::plugin_data::list(&conn, &id)?
            .into_iter()
            .map(|r| PluginDataEntry {
                key: r.key,
                value: r.value,
                updated_at: r.updated_at,
            })
            .collect::<Vec<_>>();
        Ok(entries)
    })
}

// ── GET /api/plugins/:id/data/:key ───────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/plugins/{id}/data/{key}",
    operation_id = "getPluginData",
    params(
        ("id" = String, Path, description = "Plugin ID"),
        ("key" = String, Path, description = "Data key"),
    ),
    responses(
        (status = 200, description = "Data entry", body = PluginDataEntry),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_data_get_handler(Path((id, key)): Path<(String, String)>) -> Response {
    db_json(|| {
        let conn = db::open_default()?;
        match db::plugins::get_data(&conn, &id, &key)? {
            Some(r) => Ok(PluginDataEntry {
                key: r.key,
                value: r.value,
                updated_at: r.updated_at,
            }),
            None => anyhow::bail!("key '{}' not found for plugin '{}'", key, id),
        }
    })
}

// ── PUT /api/plugins/:id/data/:key ───────────────────────────────────────────

#[utoipa::path(
    put,
    path = "/api/plugins/{id}/data/{key}",
    operation_id = "setPluginData",
    params(
        ("id" = String, Path, description = "Plugin ID"),
        ("key" = String, Path, description = "Data key"),
    ),
    request_body = SetPluginDataRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_data_set_handler(
    Path((id, key)): Path<(String, String)>,
    Json(body): Json<SetPluginDataRequest>,
) -> Response {
    db_ok(|| {
        let conn = db::open_default()?;
        db::plugin_data::set(&conn, &id, &key, &body.value)?;
        Ok(())
    })
}

// ── DELETE /api/plugins/:id/data/:key ────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/plugins/{id}/data/{key}",
    operation_id = "deletePluginData",
    params(
        ("id" = String, Path, description = "Plugin ID"),
        ("key" = String, Path, description = "Data key"),
    ),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_data_delete_handler(Path((id, key)): Path<(String, String)>) -> Response {
    db_remove("data key", &key, || {
        let conn = db::open_default()?;
        db::plugin_data::delete(&conn, &id, &key)
    })
}

// ── POST /api/plugins ─────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/plugins",
    operation_id = "installPlugin",
    request_body = PluginInstallRequest,
    responses(
        (status = 200, description = "Plugin id that was installed", body = OkResponse),
        (status = 400, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_install_handler(Json(body): Json<PluginInstallRequest>) -> Response {
    db_ok(|| {
        let instance_id = body.instance_id.as_deref();
        orca_commands::install_plugin(&body.manifest, instance_id)?;
        Ok(())
    })
}

// ── DELETE /api/plugins/:id ───────────────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/plugins/{id}",
    operation_id = "removePlugin",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_remove_handler(Path(id): Path<String>) -> Response {
    db_remove("plugin", &id, || orca_commands::remove_plugin(&id))
}

// ── PATCH /api/plugins/:id/enable ────────────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/api/plugins/{id}/enable",
    operation_id = "enablePlugin",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_enable_handler(Path(id): Path<String>) -> Response {
    db_remove("plugin", &id, || {
        let conn = db::open_default()?;
        db::plugins::set_enabled(&conn, &id, true)
    })
}

// ── PATCH /api/plugins/:id/disable ───────────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/api/plugins/{id}/disable",
    operation_id = "disablePlugin",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugins"
)]
pub async fn plugin_disable_handler(Path(id): Path<String>) -> Response {
    db_remove("plugin", &id, || {
        let conn = db::open_default()?;
        db::plugins::set_enabled(&conn, &id, false)
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
    match orca_commands::creds_cmd::sync_plugin_creds(&id) {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
