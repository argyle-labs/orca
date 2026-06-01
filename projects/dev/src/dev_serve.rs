use anyhow::{Context, Result};
use axum::{Router, extract::State, response::IntoResponse, routing::get};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;

const DEFAULT_PORT: u16 = 12009;
const DEFAULT_BINARY: &str = "target/x86_64-unknown-linux-gnu/release/orca";

#[derive(Clone)]
struct Srv {
    binary_path: Arc<PathBuf>,
    /// Cached (sha256, bytes) keyed by binary mtime. Refreshed on each request
    /// if the file has been touched since last read.
    cache: Arc<RwLock<BinaryCache>>,
}

struct BinaryCache {
    mtime: Option<std::time::SystemTime>,
    sha256: String,
    bytes: Vec<u8>,
}

impl BinaryCache {
    fn empty() -> Self {
        Self {
            mtime: None,
            sha256: String::new(),
            bytes: Vec::new(),
        }
    }
}

impl Srv {
    fn new(binary_path: PathBuf) -> Self {
        Self {
            binary_path: Arc::new(binary_path),
            cache: Arc::new(RwLock::new(BinaryCache::empty())),
        }
    }

    async fn ensure_fresh(&self) -> Result<()> {
        let current_mtime = std::fs::metadata(&*self.binary_path)
            .and_then(|m| m.modified())
            .ok();

        {
            let cached = self.cache.read().await;
            if cached.mtime.is_some() && cached.mtime == current_mtime {
                return Ok(());
            }
        }

        let bytes = std::fs::read(&*self.binary_path)
            .with_context(|| format!("read {}", self.binary_path.display()))?;
        let sha256 = utils::hash::sha256_hex(&bytes);
        let mut cached = self.cache.write().await;
        *cached = BinaryCache {
            mtime: current_mtime,
            sha256,
            bytes,
        };
        Ok(())
    }
}

async fn version_handler(State(srv): State<Srv>) -> impl IntoResponse {
    match srv.ensure_fresh().await {
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{{\"error\":\"{e}\"}}"),
        )
            .into_response(),
        Ok(()) => {
            let sha = srv.cache.read().await.sha256.clone();
            axum::Json(serde_json::json!({ "sha256": sha })).into_response()
        }
    }
}

async fn binary_handler(State(srv): State<Srv>) -> impl IntoResponse {
    match srv.ensure_fresh().await {
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("error: {e}"),
        )
            .into_response(),
        Ok(()) => {
            let bytes = srv.cache.read().await.bytes.clone();
            (
                [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                bytes,
            )
                .into_response()
        }
    }
}

pub async fn cmd_dev_serve(binary: Option<&Path>, port: u16) -> Result<()> {
    let port = if port == 0 { DEFAULT_PORT } else { port };
    let binary_path = binary
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY));

    if !binary_path.exists() {
        anyhow::bail!(
            "binary not found at {} — run `cargo build --release --target x86_64-unknown-linux-gnu` first",
            binary_path.display()
        );
    }

    let srv = Srv::new(binary_path.clone());

    // Warm the cache immediately so the first request doesn't race a build.
    srv.ensure_fresh().await?;
    {
        let c = srv.cache.read().await;
        println!(
            "[orca dev serve] serving {} ({} bytes, sha256={}...)",
            binary_path.display(),
            c.bytes.len(),
            &c.sha256[..12]
        );
    }
    println!("[orca dev serve] listening on http://0.0.0.0:{port}");
    println!("[orca dev serve]   GET /version.json  — sha256 of current build");
    println!("[orca dev serve]   GET /binary         — binary bytes");
    println!("[orca dev serve] On each peer: orca update --source http://<host-i-ip>:{port}",);

    let app = Router::new()
        .route("/version.json", get(version_handler))
        .route("/binary", get(binary_handler))
        .with_state(srv);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind to port {port}"))?;

    axum::serve(listener, app)
        .await
        .context("dev serve error")?;
    Ok(())
}
