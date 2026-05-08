//! Integration test: full TCP+mTLS round-trip against the plugin host.
//!
//! Boots a real plugin host on an ephemeral port using a fresh PKI directory,
//! connects from the SDK side as a freshly-issued plugin, and exercises
//! `orca/hello` plus an unknown-method error path.

use std::net::SocketAddr;

use orca::plugin_host;
use orca_sdk::pki::{self, Capability};
use orca_sdk::transport::{Sensitivity, TcpTransport, TypeDeclaration, TypedValue};

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
    boot_host_with_registry(pki_dir, plugin_host::ContextRegistry::new()).await
}

async fn boot_host_with_registry(
    pki_dir: &std::path::Path,
    registry: plugin_host::ContextRegistry,
) -> SocketAddr {
    let (listener, acceptor, bound) = plugin_host::bind(pki_dir, 0)
        .await
        .expect("plugin_host::bind");
    tokio::spawn(async move {
        let _ = plugin_host::serve(listener, acceptor, registry).await;
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
            vec!["orca/fs.read".to_string()], // not yet implemented
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.status, "degraded");
    assert!(result.reason.unwrap().contains("orca/fs.read"));
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
            vec!["orca/fs.read".to_string()], // required but unavailable
            vec![],
        )
        .await
        .unwrap_err();

    let msg = format!("{err:#}");
    assert!(msg.contains("rejected"), "expected rejection, got: {msg}");
    assert!(
        msg.contains("orca/fs.read"),
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
async fn context_subscribe_receives_published_events_across_clients() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let pub_bundle = pki::issue(pki_dir, "publisher", Capability::General).unwrap();
    let sub_bundle = pki::issue(pki_dir, "subscriber", Capability::General).unwrap();

    let registry = plugin_host::ContextRegistry::new();
    let addr = boot_host_with_registry(pki_dir, registry).await;

    let publisher = TcpTransport::connect(addr, &pub_bundle).await.unwrap();
    publisher
        .hello("publisher", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let subscriber = TcpTransport::connect(addr, &sub_bundle).await.unwrap();
    subscriber
        .hello("subscriber", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let (sub_id, mut events) = subscriber
        .subscribe_context("room:kitchen", vec![])
        .await
        .unwrap();
    assert!(!sub_id.is_empty());

    // Give the subscription pump task a moment to register the broadcast rx
    // before we publish; without this the publish may race ahead of subscribe.
    tokio::task::yield_now().await;

    let value = TypedValue {
        type_id: "orca.host.LoadSample".into(),
        schema_version: "0.1.0".into(),
        sensitivity: Sensitivity::General,
        payload: serde_json::json!({"cpu": 0.42}),
    };
    publisher
        .publish_context("room:kitchen", value.clone())
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .expect("event arrived in time")
        .expect("event present");

    assert_eq!(event.subscription_id, sub_id);
    assert_eq!(event.context_id, "room:kitchen");
    assert_eq!(event.value.type_id, value.type_id);
    assert_eq!(event.value.payload, value.payload);
}

#[tokio::test(flavor = "current_thread")]
async fn context_subscribe_type_filter_drops_other_types() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "selffilter", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
    transport
        .hello("selffilter", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let (_sub_id, mut events) = transport
        .subscribe_context("room:office", vec!["orca.host.LoadSample".into()])
        .await
        .unwrap();
    tokio::task::yield_now().await;

    // Publish a non-matching type — should be filtered out.
    transport
        .publish_context(
            "room:office",
            TypedValue {
                type_id: "arr.sonarr.Series".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

    // Publish a matching type — should arrive.
    transport
        .publish_context(
            "room:office",
            TypedValue {
                type_id: "orca.host.LoadSample".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({"cpu": 0.1}),
            },
        )
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .expect("matching event arrives")
        .expect("event present");
    assert_eq!(event.value.type_id, "orca.host.LoadSample");

    // Channel should now be empty (within a brief window).
    let extra = tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await;
    assert!(extra.is_err(), "no more events expected, got {extra:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn context_unsubscribe_stops_events() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "unsub-plug", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
    transport
        .hello("unsub-plug", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let (sub_id, mut events) = transport.subscribe_context("c1", vec![]).await.unwrap();
    tokio::task::yield_now().await;

    transport
        .publish_context(
            "c1",
            TypedValue {
                type_id: "t.A".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
    let _first = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .expect("first event arrives")
        .expect("event present");

    transport.unsubscribe_context(sub_id).await.unwrap();

    transport
        .publish_context(
            "c1",
            TypedValue {
                type_id: "t.A".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

    let after = tokio::time::timeout(std::time::Duration::from_millis(150), events.recv()).await;
    assert!(
        after.is_err(),
        "should not receive events after unsubscribe, got {after:?}"
    );
}

/// Stress test the response demultiplexer: fire many concurrent calls on a
/// single transport and assert each response is routed back to the matching
/// request. Uses `orca/types.declare` because each request declares a uniquely
/// named type and the response echoes the fully-qualified id back, giving us
/// content (not just request-id) to match against.
#[tokio::test(flavor = "current_thread")]
async fn concurrent_calls_demux_routes_each_response_to_its_request() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "stress-plug", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
    transport
        .hello("stress-plug", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    const N: usize = 30;

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let t = transport.clone();
        handles.push(tokio::spawn(async move {
            let type_name = format!("Type{i:02}");
            let result = t
                .declare_types(vec![TypeDeclaration {
                    type_name: type_name.clone(),
                    schema_version: "0.1.0".into(),
                    schema: serde_json::json!({
                        "type": "object",
                        "properties": { "i": { "const": i } },
                    }),
                    sensitivity: Sensitivity::General,
                }])
                .await
                .expect("declare_types call");
            (i, type_name, result)
        }));
    }

    for h in handles {
        let (i, type_name, result) = h.await.expect("task join");
        assert_eq!(
            result.accepted,
            vec![format!("stress-plug.{type_name}")],
            "call #{i} got mismatched response — demux routing failed"
        );
    }

    // Every type should have landed in the DB exactly once.
    let conn = db::open_default().unwrap();
    let rows = db::list_plugin_types(&conn, "stress-plug").unwrap();
    assert_eq!(
        rows.len(),
        N,
        "expected {N} persisted types, got {}",
        rows.len()
    );

    db::set_thread_db_path(None);
}

/// Order + completeness guarantee: when a single publisher fires a burst of
/// values to one context, a single subscriber must receive every value in
/// the exact order they were published, with no gaps and no reordering.
///
/// Guards against three classes of regression:
///   1. Reorder through the host's broadcast → pump → writer pipeline.
///   2. Silent drops when the broadcast channel laps a slow subscriber
///      (the pump's `Lagged` arm currently skips events — if this test
///      starts failing intermittently, that arm is the suspect).
///   3. TCP-level segmentation losing frame boundaries inside a burst.
#[tokio::test(flavor = "current_thread")]
async fn context_publish_subscribe_preserves_order_under_burst() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let pub_bundle = pki::issue(pki_dir, "burst-pub", Capability::General).unwrap();
    let sub_bundle = pki::issue(pki_dir, "burst-sub", Capability::General).unwrap();

    let registry = plugin_host::ContextRegistry::new();
    let addr = boot_host_with_registry(pki_dir, registry).await;

    let publisher = TcpTransport::connect(addr, &pub_bundle).await.unwrap();
    publisher
        .hello("burst-pub", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let subscriber = TcpTransport::connect(addr, &sub_bundle).await.unwrap();
    subscriber
        .hello("burst-sub", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let (_sub_id, mut events) = subscriber
        .subscribe_context("burst:room", vec![])
        .await
        .unwrap();
    // Let the subscription pump register its broadcast rx before we publish.
    tokio::task::yield_now().await;

    // 200 messages: well above any obvious batching threshold but below the
    // broadcast channel capacity (256), so no Lagged drops are expected.
    const N: u64 = 200;
    for seq in 0..N {
        publisher
            .publish_context(
                "burst:room",
                TypedValue {
                    type_id: "burst.Tick".into(),
                    schema_version: "0.1.0".into(),
                    sensitivity: Sensitivity::General,
                    payload: serde_json::json!({"seq": seq}),
                },
            )
            .await
            .unwrap();
    }

    // Drain exactly N events. Each publish_context call awaited a response,
    // so by the time the loop above completes, the host has accepted every
    // publish — but the events still need to traverse pump → writer → reader
    // → mpsc, so we time-box per recv.
    let mut received = Vec::with_capacity(N as usize);
    for i in 0..N {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for event #{i} of {N}"))
            .expect("event present");
        let seq = event
            .value
            .payload
            .get("seq")
            .and_then(|v| v.as_u64())
            .expect("seq present");
        received.push(seq);
    }

    // Strict order, no gaps, no duplicates.
    let expected: Vec<u64> = (0..N).collect();
    assert_eq!(
        received, expected,
        "out-of-order or lossy delivery: got {received:?}"
    );

    // Channel must be empty after exactly N — no late stragglers, no doubles.
    let extra = tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await;
    assert!(
        extra.is_err(),
        "unexpected extra event after {N}: {extra:?}"
    );
}

/// Cross-publisher interleaving: when two publishers publish to the same
/// context concurrently, a subscriber must see each publisher's stream in
/// its original order (the relative order between publishers is undefined,
/// but per-publisher sequencing must be preserved).
#[tokio::test(flavor = "current_thread")]
async fn context_subscribe_preserves_per_publisher_order_when_interleaved() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let pub_a = pki::issue(pki_dir, "pub-a", Capability::General).unwrap();
    let pub_b = pki::issue(pki_dir, "pub-b", Capability::General).unwrap();
    let sub_b = pki::issue(pki_dir, "ord-sub", Capability::General).unwrap();

    let registry = plugin_host::ContextRegistry::new();
    let addr = boot_host_with_registry(pki_dir, registry).await;

    let publisher_a = TcpTransport::connect(addr, &pub_a).await.unwrap();
    publisher_a
        .hello("pub-a", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();
    let publisher_b = TcpTransport::connect(addr, &pub_b).await.unwrap();
    publisher_b
        .hello("pub-b", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();
    let subscriber = TcpTransport::connect(addr, &sub_b).await.unwrap();
    subscriber
        .hello("ord-sub", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    let (_sub_id, mut events) = subscriber.subscribe_context("xc", vec![]).await.unwrap();
    tokio::task::yield_now().await;

    const N: u64 = 50;

    // Run both publishers concurrently. Each fires N sequenced messages.
    let task_a = tokio::spawn({
        let pub_a = publisher_a.clone();
        async move {
            for seq in 0..N {
                pub_a
                    .publish_context(
                        "xc",
                        TypedValue {
                            type_id: "xc.A".into(),
                            schema_version: "0.1.0".into(),
                            sensitivity: Sensitivity::General,
                            payload: serde_json::json!({"who": "a", "seq": seq}),
                        },
                    )
                    .await
                    .unwrap();
            }
        }
    });
    let task_b = tokio::spawn({
        let pub_b = publisher_b.clone();
        async move {
            for seq in 0..N {
                pub_b
                    .publish_context(
                        "xc",
                        TypedValue {
                            type_id: "xc.B".into(),
                            schema_version: "0.1.0".into(),
                            sensitivity: Sensitivity::General,
                            payload: serde_json::json!({"who": "b", "seq": seq}),
                        },
                    )
                    .await
                    .unwrap();
            }
        }
    });
    task_a.await.unwrap();
    task_b.await.unwrap();

    // Collect 2N events; per-publisher sequence numbers must be monotonically
    // increasing (relative interleaving between publishers is unconstrained).
    let mut last_a: i64 = -1;
    let mut last_b: i64 = -1;
    for i in 0..(2 * N) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out at event #{i}"))
            .expect("event present");
        let who = event
            .value
            .payload
            .get("who")
            .and_then(|v| v.as_str())
            .unwrap();
        let seq = event
            .value
            .payload
            .get("seq")
            .and_then(|v| v.as_u64())
            .unwrap() as i64;
        match who {
            "a" => {
                assert!(
                    seq > last_a,
                    "pub-a out of order: got seq={seq} after last_a={last_a}"
                );
                last_a = seq;
            }
            "b" => {
                assert!(
                    seq > last_b,
                    "pub-b out of order: got seq={seq} after last_b={last_b}"
                );
                last_b = seq;
            }
            other => panic!("unexpected publisher tag: {other}"),
        }
    }
    assert_eq!(last_a, (N - 1) as i64, "pub-a missing tail values");
    assert_eq!(last_b, (N - 1) as i64, "pub-b missing tail values");
}

/// Once a type is declared, publishing a payload that conforms to its schema
/// must succeed; publishing a non-conforming payload must be rejected.
#[tokio::test(flavor = "current_thread")]
async fn context_publish_validates_payload_against_declared_schema() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "schema-pub", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
    transport
        .hello("schema-pub", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    // Declare a type whose schema requires `cpu: number`.
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "cpu": { "type": "number" } },
        "required": ["cpu"],
    });
    transport
        .declare_types(vec![TypeDeclaration {
            type_name: "Load".into(),
            schema_version: "0.1.0".into(),
            schema,
            sensitivity: Sensitivity::General,
        }])
        .await
        .unwrap();

    // Conforming publish — must succeed.
    transport
        .publish_context(
            "ch",
            TypedValue {
                type_id: "schema-pub.Load".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({"cpu": 0.42}),
            },
        )
        .await
        .expect("conforming payload should be accepted");

    // Non-conforming publish — `cpu` is the wrong type; must be rejected.
    let err = transport
        .publish_context(
            "ch",
            TypedValue {
                type_id: "schema-pub.Load".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({"cpu": "hot"}),
            },
        )
        .await
        .expect_err("non-conforming payload should be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("schema validation"),
        "expected schema validation error, got: {msg}"
    );
    assert!(
        msg.contains("schema-pub.Load"),
        "should name the type: {msg}"
    );

    // Missing required field — also rejected.
    let err = transport
        .publish_context(
            "ch",
            TypedValue {
                type_id: "schema-pub.Load".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({}),
            },
        )
        .await
        .expect_err("missing required field should be rejected");
    assert!(
        format!("{err:#}").contains("schema validation"),
        "expected schema validation error"
    );

    db::set_thread_db_path(None);
}

/// Undeclared types currently pass through unchecked (declaration is opt-in).
/// This guards that behavior so we notice if we ever flip to strict mode.
#[tokio::test(flavor = "current_thread")]
async fn context_publish_allows_undeclared_types() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    isolate_db(pki_dir);
    pki::init(pki_dir).unwrap();
    let bundle = pki::issue(pki_dir, "undeclared-pub", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
    transport
        .hello("undeclared-pub", orca_sdk::Flavor::Headless, vec![], vec![])
        .await
        .unwrap();

    // No declare_types call — publishing should still succeed.
    transport
        .publish_context(
            "ch",
            TypedValue {
                type_id: "some.unknown.Type".into(),
                schema_version: "0.1.0".into(),
                sensitivity: Sensitivity::General,
                payload: serde_json::json!({"anything": true}),
            },
        )
        .await
        .expect("undeclared type should pass through");

    db::set_thread_db_path(None);
}

/// Spoofing guard: a plugin holding a CA-signed cert for "alpha" must not be
/// able to claim plugin_id "beta" in `orca/hello`. The plugin host extracts
/// the CN from the peer's leaf cert and rejects any mismatched claim.
#[tokio::test(flavor = "current_thread")]
async fn hello_rejects_when_plugin_id_does_not_match_peer_cert_cn() {
    install_ring();

    let dir = tempfile::tempdir().unwrap();
    let pki_dir = dir.path();
    pki::init(pki_dir).unwrap();
    // Cert is for "alpha"…
    let alpha_bundle = pki::issue(pki_dir, "alpha", Capability::General).unwrap();

    let addr = boot_host(pki_dir).await;
    let transport = TcpTransport::connect(addr, &alpha_bundle).await.unwrap();

    // …but the hello claims to be "beta". Should be rejected as ok=false.
    let params = serde_json::json!({
        "sdk_version": "0.1.0",
        "plugin_id": "beta",
        "flavor": "headless",
        "core_min_required": "0.1.0",
        "methods_required": [],
        "methods_optional": [],
    });
    let resp = transport.call("orca/hello", Some(params)).await.unwrap();
    assert!(
        !resp.is_error(),
        "identity mismatch should return ok=false, not RPC error"
    );

    let result: orca_sdk::transport::HelloResult =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!result.ok, "expected ok=false");
    assert_eq!(result.status, "rejected");
    let reason = result.reason.unwrap();
    assert!(
        reason.contains("beta"),
        "reason should mention claimed id: {reason}"
    );
    assert!(
        reason.contains("alpha"),
        "reason should mention actual CN: {reason}"
    );

    // And subsequent calls that require prior hello should still be rejected
    // — the spoofed hello must not have populated plugin_id.
    let resp = transport
        .call("orca/types.declare", Some(serde_json::json!({"types": []})))
        .await
        .unwrap();
    assert!(resp.is_error());
    assert!(resp.error.unwrap().message.contains("orca/hello"));
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
