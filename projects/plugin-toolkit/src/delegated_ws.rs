//! Delegated WebSocket: a subprocess plugin talks WebSocket through orca's
//! `ws.*` capabilities, so it links no WebSocket or TLS stack. The daemon owns
//! the socket (and answers pings); the plugin holds a session id.
//!
//! Each call is a capability round-trip on the installed cap sink, so a
//! [`WsClient`] works only while a tool invocation is running. Sessions a
//! plugin never closes are dropped by the daemon when the plugin exits; a
//! dropped [`WsClient`] closes its session best-effort.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::abi::{WsChunk, WsClose, WsOpen, WsPayload, WsRecv, WsSend};
use crate::capsink::cap_route;

fn call<T: DeserializeOwned>(cap: &str, op: &impl Serialize) -> Result<T> {
    match cap_route(cap, &serde_json::to_string(op)?) {
        Some(reply) => Ok(serde_json::from_str(&reply?)?),
        None => Err(anyhow!(
            "{cap} capability unavailable: this plugin is not running as an orca subprocess"
        )),
    }
}

/// One host-brokered WebSocket session.
#[derive(Debug)]
pub struct WsClient {
    session: u64,
}

impl WsClient {
    /// Open `req.url` (`ws://` / `wss://`) through the daemon. Set
    /// `req.max_message_bytes` for peers that send messages over 1 MiB.
    pub async fn connect(req: WsOpen) -> Result<Self> {
        match call("ws.open", &req)? {
            WsChunk::Open { session } => Ok(Self { session }),
            other => bail!("ws.open: unexpected reply {other:?}"),
        }
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    pub async fn send(&self, text: String) -> Result<()> {
        self.send_payload(WsPayload::Text(text))
    }

    pub async fn send_binary(&self, bytes: Vec<u8>) -> Result<()> {
        self.send_payload(WsPayload::Binary(bytes))
    }

    fn send_payload(&self, payload: WsPayload) -> Result<()> {
        call::<serde::de::IgnoredAny>(
            "ws.send",
            &WsSend {
                session: self.session,
                payload,
            },
        )
        .map(drop)
    }

    /// Next text message; `None` once the peer closed. A binary message is an
    /// error here — use [`recv_chunk`](Self::recv_chunk) on mixed streams.
    pub async fn recv(&self) -> Result<Option<String>> {
        match self.recv_chunk().await? {
            None => Ok(None),
            Some(WsChunk::Text { text }) => Ok(Some(text)),
            Some(other) => bail!("ws.recv: expected a text message, got {other:?}"),
        }
    }

    /// Next message of any kind; `None` once the peer closed. Errors on a
    /// dropped connection or when nothing arrives within the daemon's timeout.
    pub async fn recv_chunk(&self) -> Result<Option<WsChunk>> {
        self.recv_with(None)
    }

    /// [`recv_chunk`](Self::recv_chunk) waiting at most `timeout`. The daemon
    /// clamps it to its own maximum; timing out keeps the session open.
    pub async fn recv_timeout(&self, timeout: Duration) -> Result<Option<WsChunk>> {
        self.recv_with(Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)))
    }

    fn recv_with(&self, timeout_ms: Option<u64>) -> Result<Option<WsChunk>> {
        call(
            "ws.recv",
            &WsRecv {
                session: self.session,
                timeout_ms,
            },
        )
    }

    /// Close with code 1000.
    pub async fn close(self) -> Result<()> {
        let session = self.session;
        std::mem::forget(self);
        close_session(session)
    }
}

fn close_session(session: u64) -> Result<()> {
    call::<serde::de::IgnoredAny>(
        "ws.close",
        &WsClose {
            session,
            code: None,
        },
    )
    .map(drop)
}

impl Drop for WsClient {
    fn drop(&mut self) {
        _ = close_session(self.session);
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    use crate::capsink::with_cap_sink;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    type Calls = Rc<RefCell<Vec<(String, Value)>>>;

    /// A cap sink that records each `(cap, op)` and answers from `replies`.
    fn fake_sink(
        replies: Vec<std::result::Result<Value, &'static str>>,
    ) -> (crate::capsink::CapSink, Calls) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&calls);
        let mut replies: VecDeque<_> = replies.into();
        let sink = Box::new(move |cap: &str, op: &str| {
            seen.borrow_mut()
                .push((cap.to_string(), serde_json::from_str(op).unwrap()));
            match replies.pop_front().expect("unexpected cap call") {
                Ok(v) => Ok(v.to_string()),
                Err(e) => Err(e.to_string()),
            }
        });
        (sink, calls)
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn round_trip_over_the_cap_sink() {
        let (sink, calls) = fake_sink(vec![
            Ok(json!({"t": "open", "session": 9})),
            Ok(Value::Null),
            Ok(json!({"t": "text", "text": "pong"})),
            Ok(Value::Null),
            Ok(Value::Null),
        ]);
        with_cap_sink(sink, || {
            block_on(async {
                let ws = WsClient::connect(WsOpen {
                    url: "wss://example.test/ws".into(),
                    headers: vec![("authorization".into(), "Bearer t".into())],
                    insecure: false,
                    timeout_ms: None,
                    max_message_bytes: Some(4 << 20),
                })
                .await
                .unwrap();
                assert_eq!(ws.session(), 9);
                ws.send("ping".into()).await.unwrap();
                assert_eq!(ws.recv().await.unwrap().as_deref(), Some("pong"));
                assert_eq!(ws.recv().await.unwrap(), None);
                ws.close().await.unwrap();
            })
        });
        let calls = calls.borrow();
        let caps: Vec<&str> = calls.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(
            caps,
            ["ws.open", "ws.send", "ws.recv", "ws.recv", "ws.close"]
        );
        assert_eq!(calls[0].1["url"], "wss://example.test/ws");
        assert_eq!(calls[0].1["max_message_bytes"], 4 << 20);
        assert_eq!(calls[1].1, json!({"session": 9, "text": "ping"}));
        assert_eq!(calls[4].1, json!({"session": 9, "code": null}));
    }

    #[test]
    fn host_errors_surface() {
        let (sink, _) = fake_sink(vec![
            Ok(json!({"t": "open", "session": 1})),
            Err("ws: connection dropped"),
            Err("ws.close: no open session 1"),
        ]);
        let err = with_cap_sink(sink, || {
            block_on(async {
                let ws = WsClient::connect(WsOpen {
                    url: "ws://example.test".into(),
                    headers: vec![],
                    insecure: false,
                    timeout_ms: None,
                    max_message_bytes: None,
                })
                .await?;
                ws.recv().await
            })
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("connection dropped"), "got: {err}");
    }

    #[test]
    fn binary_message_is_refused_by_text_recv() {
        let (sink, _) = fake_sink(vec![
            Ok(json!({"t": "binary", "bytes": "AP8="})),
            Ok(Value::Null),
        ]);
        let err = with_cap_sink(sink, || block_on(WsClient { session: 1 }.recv()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected a text message"), "got: {err}");
    }

    #[test]
    fn without_a_sink_errors() {
        let err = block_on(WsClient { session: 1 }.send("x".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not running as an orca subprocess"),
            "got: {err}"
        );
    }

    #[test]
    fn drop_closes_the_session_once() {
        let (sink, calls) = fake_sink(vec![Ok(Value::Null), Ok(Value::Null)]);
        with_cap_sink(sink, || {
            drop(WsClient { session: 4 });
            block_on(WsClient { session: 5 }.close()).unwrap();
        });
        let calls = calls.borrow();
        assert_eq!(
            *calls,
            [
                ("ws.close".to_string(), json!({"session": 4, "code": null})),
                ("ws.close".to_string(), json!({"session": 5, "code": null})),
            ]
        );
    }

    #[test]
    fn recv_timeout_sends_the_wait() {
        let (sink, calls) = fake_sink(vec![Ok(json!({"t": "text", "text": "x"})), Ok(Value::Null)]);
        let got = with_cap_sink(sink, || {
            block_on(WsClient { session: 2 }.recv_timeout(Duration::from_millis(1500)))
        })
        .unwrap();
        assert_eq!(got, Some(WsChunk::Text { text: "x".into() }));
        assert_eq!(
            calls.borrow()[0].1,
            json!({"session": 2, "timeout_ms": 1500})
        );
    }
}
