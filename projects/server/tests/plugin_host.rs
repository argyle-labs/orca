//! Integration test: full TCP+mTLS round-trip against the plugin host.
//!
//! Boots a real plugin host on an ephemeral port using a fresh PKI directory,
//! connects from the SDK side as a freshly-issued plugin, and exercises
//! `orca/hello` plus an unknown-method error path.

use std::net::SocketAddr;

use orca::plugin_host;
use orca_sdk::pki::{self, Capability};
use orca_sdk::transport::TcpTransport;

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
            vec!["orca/types.declare".to_string()], // not yet implemented
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.status, "degraded");
    assert!(result.reason.unwrap().contains("orca/types.declare"));
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
            vec!["orca/types.declare".to_string()], // required but unavailable
            vec![],
        )
        .await
        .unwrap_err();

    let msg = format!("{err:#}");
    assert!(msg.contains("rejected"), "expected rejection, got: {msg}");
    assert!(
        msg.contains("orca/types.declare"),
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
