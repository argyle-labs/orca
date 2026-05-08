//! Integration test: full TCP+mTLS round-trip against the plugin host.
//!
//! Boots a real plugin host on an ephemeral port using a fresh PKI directory,
//! connects from the SDK side as a freshly-issued plugin, and exercises
//! `orca/hello` plus an unknown-method error path.

use std::net::SocketAddr;

use orca::plugin_host;
use orca_sdk::pki::{self, Capability};
use orca_sdk::transport::{Sensitivity, TcpTransport, TypeDeclaration};

/// Point the db crate at an isolated SQLite file for the lifetime of the test.
/// Must be called before any code path that opens the DB.
fn isolate_db(dir: &std::path::Path) {
    let db_path = dir.join("orca.db");
    db::set_thread_db_path(Some(db_path.to_str().unwrap()));
}

fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn boot_host(pki_dir: &std::path::Path) -> SocketAddr {
    let (listener, acceptor, bound) = plugin_host::bind(pki_dir, 0)
        .await
        .expect("plugin_host::bind");
    tokio::spawn(async move {
        let _ = plugin_host::serve(listener, acceptor).await;
    });
    // Connect via loopback regardless of the 0.0.0.0 bind.
    SocketAddr::from(([127, 0, 0, 1], bound.port()))
}

#[tokio::test(flavor = "current_thread")]
async fn hello_round_trip() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    pki::init(pki_dir).unwrap();
    let plugin_bundle = pki::issue(pki_dir, "it-plugin", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;

    let transport = TcpTransport::connect(addr, &plugin_bundle).await.unwrap();
    let result = transport
        .hello(
            "it-plugin",
            orca_sdk::Flavor::Headless,
            vec!["orca/hello".to_string()],
            vec![],
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.status, "full");
    assert!(result.methods.iter().any(|m| m == "orca/hello"));
    assert!(!result.server_version.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn hello_degraded_when_optional_method_missing() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "p-deg", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();

    let result = transport
        .hello(
            "p-deg",
            orca_sdk::Flavor::Headless,
            vec!["orca/hello".to_string()],
            vec!["orca/context.subscribe".to_string()], // not yet implemented
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.status, "degraded");
    assert!(result.reason.unwrap().contains("orca/context.subscribe"));
}

#[tokio::test(flavor = "current_thread")]
async fn hello_rejects_when_required_method_missing() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "p-rej", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();

    let err = transport
        .hello(
            "p-rej",
            orca_sdk::Flavor::Headless,
            vec!["orca/context.subscribe".to_string()], // required but unavailable
            vec![],
        )
        .await
        .unwrap_err();

    let msg = format!("{err:#}");
    assert!(msg.contains("rejected"), "expected rejection, got: {msg}");
    assert!(
        msg.contains("orca/context.subscribe"),
        "expected method name in reason: {msg}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hello_rejects_when_server_version_too_low() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "p-ver", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();

    // Fabricate a hello with an absurdly high core_min_required.
    let params = serde_json::json!({
        "sdk_version": "0.1.0",
        "plugin_id": "p-ver",
        "flavor": "headless",
        "core_min_required": "999.0.0",
        "methods_required": [],
        "methods_optional": [],
    });
    let resp = transport.call("orca/hello", Some(params)).await.unwrap();
    assert!(
        !resp.is_error(),
        "version mismatch should return ok=false, not RPC error"
    );

    let result: orca_sdk::transport::HelloResult =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!result.ok);
    assert_eq!(result.status, "rejected");
    assert!(result.reason.unwrap().contains("999.0.0"));
}

#[tokio::test(flavor = "current_thread")]
async fn types_declare_persists_and_returns_accepted() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "type-decl", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
    transport
        .hello("type-decl", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let schema = serde_json::json!({
        "type": "object",
        "properties": { "title": { "type": "string" } },
        "required": ["title"],
    });
    let result = transport
        .declare_types(vec![
            TypeDeclaration {
                type_name: "Series".into(),
                schema_version: "0.1.0".into(),
                schema: schema.clone(),
                sensitivity: Sensitivity::General,
            },
            TypeDeclaration {
                type_name: "Episode".into(),
                schema_version: "0.1.0".into(),
                schema: serde_json::json!({"type": "object"}),
                sensitivity: Sensitivity::Sensitive,
            },
        ])
        .await
        .unwrap();

    assert_eq!(
        result.accepted,
        vec![
            "type-decl.Series".to_string(),
            "type-decl.Episode".to_string()
        ]
    );

    // Verify persistence directly via the DB.
    let conn = db::open_default().unwrap();
    let rows = db::list_plugin_types(&conn, "type-decl").unwrap();
    assert_eq!(rows.len(), 2);
    let series = rows.iter().find(|r| r.type_name == "Series").unwrap();
    assert_eq!(series.fq_type_id, "type-decl.Series");
    assert_eq!(series.schema_version, "0.1.0");
    assert_eq!(series.sensitivity, "general");
    let parsed: serde_json::Value = serde_json::from_str(&series.schema_json).unwrap();
    assert_eq!(parsed, schema);

    let episode = rows.iter().find(|r| r.type_name == "Episode").unwrap();
    assert_eq!(episode.sensitivity, "sensitive");

    db::set_thread_db_path(None);
}

#[tokio::test(flavor = "current_thread")]
async fn types_declare_requires_prior_hello() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "no-hello", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();

    let resp = transport
        .call("orca/types.declare", Some(serde_json::json!({"types": []})))
        .await
        .unwrap();

    assert!(resp.is_error());
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("orca/hello"));

    db::set_thread_db_path(None);
}

#[tokio::test(flavor = "current_thread")]
async fn types_declare_upserts_on_resubmit() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "upsert-plug", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
    transport
        .hello("upsert-plug", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    transport
        .declare_types(vec![TypeDeclaration {
            type_name: "Thing".into(),
            schema_version: "0.1.0".into(),
            schema: serde_json::json!({"type": "object"}),
            sensitivity: Sensitivity::General,
        }])
        .await
        .unwrap();

    transport
        .declare_types(vec![TypeDeclaration {
            type_name: "Thing".into(),
            schema_version: "0.2.0".into(),
            schema: serde_json::json!({"type": "object", "additionalProperties": false}),
            sensitivity: Sensitivity::Sensitive,
        }])
        .await
        .unwrap();

    let conn = db::open_default().unwrap();
    let row = db::get_plugin_type(&conn, "upsert-plug.Thing")
        .unwrap()
        .unwrap();
    assert_eq!(row.schema_version, "0.2.0");
    assert_eq!(row.sensitivity, "sensitive");

    db::set_thread_db_path(None);
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_method_returns_error() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    pki::init(pki_dir).unwrap();
    let plugin_bundle = pki::issue(pki_dir, "it-plugin-2", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;

    let transport = TcpTransport::connect(addr, &plugin_bundle).await.unwrap();
    let resp = transport.call("orca/does-not-exist", None).await.unwrap();

    assert!(resp.is_error());
    let err = resp.error.expect("error object");
    assert_eq!(err.code, -32601);
}
