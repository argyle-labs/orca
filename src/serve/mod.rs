mod api;
mod mcp_client;
mod openapi;
pub mod tree;

use std::net::SocketAddr;

use anyhow::Result;
use axum::Router;
use axum::routing::{get, post};
use tower_http::cors::{Any, CorsLayer};

pub async fn run(dev: bool, port: u16) -> Result<()> {
    let app = build_router(dev);

    let addr: SocketAddr = if dev {
        format!("127.0.0.1:{port}").parse()?
    } else {
        format!("0.0.0.0:{port}").parse()?
    };

    println!("[brain] binding {}...", addr);
    let listener = tokio::net::TcpListener::bind(addr).await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e} — is port {port} already in use?"))?;
    println!("[brain] listening on http://localhost:{port}");

    if !dev {
        println!("[brain] serving static site from site/dist");
    }

    axum::serve(listener, app).await?;
    Ok(())
}

fn build_router(dev: bool) -> Router {
    use std::sync::Arc;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mcp_pool = Arc::new(mcp_client::McpPool::new());

    let api = Router::new()
        // brain's own spec — separate from the external registry
        .route("/api/openapi.json", get(openapi::openapi_handler))
        .route("/api/openapi/public.json", get(openapi::openapi_public_handler))
        // external repo spec registry
        .route("/api/specs", get(api::specs_list_handler))
        .route("/api/specs/:repo/public", get(api::specs_get_public_handler))
        .route("/api/specs/:repo", get(api::specs_get_handler))
        .route("/api/tree", get(api::tree_handler))
        .route("/api/search", get(api::search_handler))
        .route("/api/mcp/tools", get(api::mcp_tools_handler))
        .route("/api/mcp/run", post(api::mcp_run_handler))
        .route("/api/docker/services", get(api::docker_services_handler))
        .route("/api/docker/action", post(api::docker_action_handler))
        .route("/api/ctx7", get(api::ctx7_handler))
        .route("/api/doc", get(api::doc_handler))
        .route("/api/schema", get(api::schema_handler))
        .route("/api/schema/domains", get(api::schema_domains_handler))
        .route("/api/rebuy/health/local", get(api::rebuy_health_handler))
        .route("/api/logs/services", get(api::log_services_handler))
        .route("/api/logs", get(api::log_fetch_handler))
        .with_state(mcp_pool)
        .layer(cors);

    if dev {
        api
    } else {
        let dist = std::env::current_dir().unwrap_or_default().join("site/dist");
        use tower_http::services::{ServeDir, ServeFile};
        let serve_dir = ServeDir::new(&dist)
            .not_found_service(ServeFile::new(dist.join("index.html")));
        api.fallback_service(serve_dir)
    }
}
