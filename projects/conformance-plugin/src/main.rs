//! Rust reference conformance plugin.
//!
//! Reads four env vars set by `orca_sdk::conformance::run_subprocess`:
//!
//!   ORCA_PLUGIN_ADDR    — host:port of the conformance host (TCP+mTLS)
//!   ORCA_PKI_DIR        — directory holding CA + this plugin's cert/key
//!   ORCA_PLUGIN_ID      — id to claim in `orca/hello` (matches cert CN)
//!   ORCA_MANIFEST_PATH  — path to the canonical manifest fixture
//!
//! Performs the canonical scenario:
//!   1. Parse the manifest from `ORCA_MANIFEST_PATH` using the SDK parser.
//!   2. Connect over mTLS, send `orca/hello`.
//!   3. Declare the `Greeting` type with the scenario's schema.
//!   4. Publish a `Greeting` value whose payload echoes
//!      `manifest.plugin.id` — proving the manifest was actually parsed.
//!   5. Exit 0.
//!
//! Every Go/Kotlin/TS port reproduces this binary's behavior.

use anyhow::{Context, Result, anyhow};
use orca_sdk::conformance::SCENARIO;
use orca_sdk::manifest;
use orca_sdk::pki;
use orca_sdk::transport::{Sensitivity, TcpTransport, TypeDeclaration, TypedValue};
use std::net::SocketAddr;
use std::path::PathBuf;

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
    let manifest_path = PathBuf::from(env_required("ORCA_MANIFEST_PATH")?);

    // 1. Parse the manifest. If this fails we never connect — the host
    //    will see the absence of `hello` and report a non-conformant run.
    let mf = manifest::parse_path(&manifest_path).context("parse manifest")?;
    if mf.plugin.id != plugin_id {
        return Err(anyhow!(
            "manifest plugin.id '{}' != ORCA_PLUGIN_ID '{}'",
            mf.plugin.id,
            plugin_id
        ));
    }

    // 2. Load this plugin's cert bundle and connect over mTLS.
    let bundle = pki::load_plugin(&pki_dir, &plugin_id).context("load plugin bundle")?;
    let transport = TcpTransport::connect(addr, &bundle)
        .await
        .context("connect to conformance host")?;

    transport
        .hello(&plugin_id, orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .context("orca/hello")?;

    // 3. Declare the scenario's type with its canonical schema.
    let schema: serde_json::Value =
        serde_json::from_str(SCENARIO.type_schema_json).context("parse scenario schema")?;
    transport
        .declare_types(vec![TypeDeclaration {
            type_name: SCENARIO.type_name.into(),
            schema_version: SCENARIO.type_schema_version.into(),
            schema,
            sensitivity: Sensitivity::General,
        }])
        .await
        .context("orca/types.declare")?;

    // 4. Publish a value. The payload echoes the manifest plugin id so the
    //    conformance checker can verify the manifest was actually parsed.
    transport
        .publish_context(
            SCENARIO.context_id,
            TypedValue {
                type_id: format!("{}.{}", plugin_id, SCENARIO.type_name),
                schema_version: SCENARIO.type_schema_version.into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({
                    "text": "hello from the Rust conformance plugin",
                    SCENARIO.manifest_id_payload_key: mf.plugin.id,
                }),
            },
        )
        .await
        .context("orca/context.publish")?;

    Ok(())
}
