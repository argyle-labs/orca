//! `system-info` — a working orca plugin example.
//!
//! Polls the local machine via `sysinfo` once per `INTERVAL`, publishes a
//! `SystemSnapshot` TypedValue into the `system:metrics` context. The orca
//! host's dashboard (or any subscriber) sees a live feed of CPU/memory.
//!
//! Run standalone for dev:
//!
//! ```sh
//! ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
//! ORCA_PKI_DIR=$HOME/.orca/pki \
//! ORCA_PLUGIN_ID=system-info \
//! cargo run -p orca-example-system-info
//! ```
//!
//! The host normally injects these env vars when it lazy-spawns the plugin;
//! manifest-driven launch is the production path.

use anyhow::{Context, Result, anyhow};
use orca_sdk::pki;
use orca_sdk::transport::{Sensitivity, TcpTransport, TypeDeclaration, TypedValue};
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use sysinfo::System;

const TYPE_NAME: &str = "SystemSnapshot";
const SCHEMA_VERSION: &str = "0.1.0";
const CONTEXT_ID: &str = "system:metrics";
const INTERVAL: Duration = Duration::from_secs(5);

const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "host": { "type": "string" },
        "uptime_sec": { "type": "integer" },
        "load_1m": { "type": "number" },
        "load_5m": { "type": "number" },
        "load_15m": { "type": "number" },
        "memory_total": { "type": "integer" },
        "memory_used": { "type": "integer" },
        "cpu_count": { "type": "integer" }
    },
    "required": ["host", "uptime_sec", "memory_total", "memory_used", "cpu_count"]
}"#;

fn env_required(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("required env var {name} not set"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("install rustls ring provider"))?;

    let addr: SocketAddr = env_required("ORCA_PLUGIN_ADDR")?
        .parse()
        .context("parse ORCA_PLUGIN_ADDR")?;
    let pki_dir = PathBuf::from(env_required("ORCA_PKI_DIR")?);
    let plugin_id = env_required("ORCA_PLUGIN_ID")?;

    let bundle = pki::load_plugin(&pki_dir, &plugin_id).context("load plugin bundle")?;
    let transport = TcpTransport::connect(addr, &bundle)
        .await
        .context("connect to orca host")?;

    transport
        .hello(&plugin_id, orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .context("orca/hello")?;

    let schema: serde_json::Value = serde_json::from_str(SCHEMA).context("parse schema")?;
    transport
        .declare_types(vec![TypeDeclaration {
            type_name: TYPE_NAME.into(),
            schema_version: SCHEMA_VERSION.into(),
            schema,
            sensitivity: Sensitivity::General,
        }])
        .await
        .context("orca/types.declare")?;

    let mut sys = System::new_all();
    let host = System::host_name().unwrap_or_else(|| "unknown".into());
    let cpu_count = sys.cpus().len();
    let type_id = format!("{plugin_id}.{TYPE_NAME}");

    loop {
        sys.refresh_memory();
        sys.refresh_cpu_usage();
        let load = System::load_average();
        let payload = json!({
            "host": host,
            "uptime_sec": System::uptime(),
            "load_1m": load.one,
            "load_5m": load.five,
            "load_15m": load.fifteen,
            "memory_total": sys.total_memory(),
            "memory_used": sys.used_memory(),
            "cpu_count": cpu_count,
        });

        transport
            .publish_context(
                CONTEXT_ID,
                TypedValue {
                    type_id: type_id.clone(),
                    schema_version: SCHEMA_VERSION.into(),
                    sensitivity: Sensitivity::General,
                    payload,
                },
            )
            .await
            .context("orca/context.publish")?;

        tokio::time::sleep(INTERVAL).await;
    }
}
