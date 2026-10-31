//! Integration tier: `/api/subscribe/fleet-update`, the client-facing WS push
//! for fleet-update progress.
//!
//! Unlike the rest of this tier, a WS upgrade needs a real socket — `oneshot`
//! against the router cannot complete an HTTP Upgrade. So the happy-path test
//! binds a real loopback listener and drives it with a real `tokio-tungstenite`
//! client, proving the wire format end to end rather than in-process.

mod common;

use common::{
    mint_admin_token, mint_member_session_cookie, oneshot_raw_with_headers, with_isolated_env,
};
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

/// No credentials presented at all must be refused — same gate as every other
/// `/api/*` route (`require_auth`), not a hole opened for this one endpoint.
#[tokio::test]
async fn fleet_update_ws_requires_auth() {
    let env = with_isolated_env();
    let (status, _) = oneshot_raw_with_headers(
        env.router(),
        "GET",
        "/api/subscribe/fleet-update",
        None,
        None,
        &[
            ("connection", HeaderValue::from_static("Upgrade")),
            ("upgrade", HeaderValue::from_static("websocket")),
            (
                "sec-websocket-key",
                HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
            ),
            ("sec-websocket-version", HeaderValue::from_static("13")),
        ],
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
}

/// An authenticated but under-privileged (`member`) caller must be refused
/// before the WS upgrade — this endpoint streams the same live fleet
/// host/version/update-status data `system.update.status` reports, and must
/// sit behind an admin bar even though that tool itself is read-shaped and
/// only requires `"any"` by default.
#[tokio::test]
async fn fleet_update_ws_requires_admin_role() {
    let env = with_isolated_env();
    let cookie = mint_member_session_cookie(&env).await;
    let (status, _) = oneshot_raw_with_headers(
        env.router(),
        "GET",
        "/api/subscribe/fleet-update",
        None,
        None,
        &[
            ("cookie", HeaderValue::from_str(&cookie).unwrap()),
            ("connection", HeaderValue::from_static("Upgrade")),
            ("upgrade", HeaderValue::from_static("websocket")),
            (
                "sec-websocket-key",
                HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
            ),
            ("sec-websocket-version", HeaderValue::from_static("13")),
        ],
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
}

/// A published `FleetUpdateEvent` round-trips to a real WS client connecting
/// with a valid bearer token.
#[tokio::test]
async fn fleet_update_ws_round_trips_a_published_event() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = env
        .router()
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mut req = format!("ws://{addr}/api/subscribe/fleet-update")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

    // Published AFTER the socket connects — the bus has no backfill, so the
    // client would hang forever waiting on anything published earlier.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    system::mesh::subscribe::publish_fleet_update(system::mesh::subscribe::FleetUpdateEvent {
        payload: "test-ws-roundtrip".into(),
    });

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for the published event")
        .expect("stream ended")
        .expect("ws frame ok");
    assert_eq!(
        msg,
        tokio_tungstenite::tungstenite::Message::Text("test-ws-roundtrip".into())
    );

    _ = ws.close(None).await;
}
