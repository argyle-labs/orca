//! Out-of-process capability sink — the plugin end of the capability channel.
//!
//! In the subprocess model there is no `set_host` FFI. Instead the toolkit's
//! serve loop installs a **capability sink** — a closure over the session socket
//! — for the duration of each `Invoke`, and delegated operations
//! (`db.op` / `secret.op` / `http.request`) route their typed payload through it
//! as a capability round-trip to orca.
//!
//! The sink is valid exactly while a tool is executing, so tool code deep in the
//! call graph reaches the socket without threading a channel through every
//! signature. Lookup order: the current thread's sink ([`with_cap_sink`]), then
//! the process-wide sink ([`with_process_cap_sink`]) that a subprocess plugin's
//! serve loop installs, so calls from `spawn_blocking` / spawned tasks / any other
//! thread of the plugin reach it too. When neither is installed (in-process
//! cdylib, or in-core `endpoint_resource!`) callers fall through to their FFI /
//! pooled paths.
//!
//! This module is dependency-light on purpose (serde_json + the ABI types only,
//! no rusqlite): the delegated-HTTP shim reaches [`http_request`] without pulling
//! the `db` feature.

use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};

use crate::abi::{HttpRequest, HttpResponse};

/// A capability round-trip: `(cap_name, op_json) -> reply_json | error`.
pub type CapSink = Box<dyn FnMut(&str, &str) -> std::result::Result<String, String>>;

/// A STREAMING capability sink: `(cap_name, op_json, on_chunk) -> () | error`.
/// The sink drives the capability and calls `on_chunk(seq, chunk_json)` for each
/// `CapStreamChunk` as it arrives, returning once the stream ends. `on_chunk`'s
/// own `Err` aborts consumption. Installed by the serve loop alongside [`CapSink`].
pub type CapStreamSink = Box<
    dyn FnMut(
        &str,
        &str,
        &mut dyn FnMut(u64, String) -> std::result::Result<(), String>,
    ) -> std::result::Result<(), String>,
>;

/// A [`CapSink`] callable from any thread, for [`with_process_cap_sink`].
pub type SharedCapSink =
    Arc<dyn Fn(&str, &str) -> std::result::Result<String, String> + Send + Sync>;

/// A [`CapStreamSink`] callable from any thread, for [`with_process_cap_stream_sink`].
pub type SharedCapStreamSink = Arc<
    dyn Fn(
            &str,
            &str,
            &mut dyn FnMut(u64, String) -> std::result::Result<(), String>,
        ) -> std::result::Result<(), String>
        + Send
        + Sync,
>;

// Thread-local `Some` overrides the process-wide caps core sent in `Welcome`.
thread_local! {
    static CORE_BODY_B64: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static CORE_NO_FOLLOW: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static CORE_PRIVILEGED: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

static PROCESS_CORE_CAPS: RwLock<Vec<String>> = RwLock::new(Vec::new());

const PRIVILEGED: [&str; 4] = [
    crate::abi::LXC_EXEC,
    crate::abi::LXC_PUSH,
    crate::abi::PRIVILEGED_APPLY,
    crate::abi::TOOL_CALL,
];

fn process_core_has(cap: &str) -> bool {
    PROCESS_CORE_CAPS
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|c| c == cap)
}

/// Record the capabilities core advertised in `Welcome` for every thread of the
/// process.
#[cfg(all(feature = "tools", feature = "endpoints"))]
pub(crate) fn set_process_core_caps(caps: &[String]) {
    *PROCESS_CORE_CAPS.write().unwrap_or_else(|p| p.into_inner()) = caps.to_vec();
}

#[cfg(any(
    test,
    feature = "testing",
    all(feature = "tools", feature = "endpoints")
))]
pub(crate) fn set_core_privileged(caps: &[String]) {
    let caps = caps
        .iter()
        .filter(|cap| PRIVILEGED.contains(&cap.as_str()))
        .cloned()
        .collect();
    CORE_PRIVILEGED.with(|c| *c.borrow_mut() = Some(caps));
}

pub fn core_advertises(cap: &str) -> bool {
    CORE_PRIVILEGED
        .with(|c| c.borrow().as_ref().map(|v| v.iter().any(|c| c == cap)))
        .unwrap_or_else(|| PRIVILEGED.contains(&cap) && process_core_has(cap))
}

fn core_body_b64() -> bool {
    CORE_BODY_B64
        .with(|c| c.get())
        .unwrap_or_else(|| process_core_has(crate::abi::HTTP_BODY_B64))
}

fn core_no_follow() -> bool {
    CORE_NO_FOLLOW
        .with(|c| c.get())
        .unwrap_or_else(|| process_core_has(crate::abi::HTTP_NO_FOLLOW))
}

/// Run a privileged capability through core when core advertises `cap` and a
/// sink is installed; `None` means the caller must take its direct path.
pub(crate) fn privileged_route(
    cap: &str,
    op_json: &str,
) -> Option<Result<crate::abi::HelperOutput>> {
    if !core_advertises(cap) {
        return None;
    }
    cap_route(cap, op_json).map(|reply| Ok(serde_json::from_str(&reply?)?))
}

#[cfg(any(
    test,
    feature = "testing",
    all(feature = "tools", feature = "endpoints")
))]
pub(crate) fn set_core_body_b64(on: bool) {
    CORE_BODY_B64.with(|c| c.set(Some(on)));
}

#[cfg(any(
    test,
    feature = "testing",
    all(feature = "tools", feature = "endpoints")
))]
pub(crate) fn set_core_no_follow(on: bool) {
    CORE_NO_FOLLOW.with(|c| c.set(Some(on)));
}

#[cfg(any(test, feature = "testing"))]
pub fn with_core_caps<R>(caps: &[&str], body: impl FnOnce() -> R) -> R {
    struct Restore(Option<bool>, Option<bool>, Option<Vec<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CORE_BODY_B64.with(|c| c.set(self.0));
            CORE_NO_FOLLOW.with(|c| c.set(self.1));
            CORE_PRIVILEGED.with(|c| *c.borrow_mut() = self.2.take());
        }
    }
    let _restore = Restore(
        CORE_BODY_B64.with(|c| c.get()),
        CORE_NO_FOLLOW.with(|c| c.get()),
        CORE_PRIVILEGED.with(|c| c.borrow().clone()),
    );
    let caps: Vec<String> = caps.iter().map(|c| c.to_string()).collect();
    set_core_body_b64(caps.iter().any(|c| c == crate::abi::HTTP_BODY_B64));
    set_core_no_follow(caps.iter().any(|c| c == crate::abi::HTTP_NO_FOLLOW));
    set_core_privileged(&caps);
    body()
}

fn request_wire(req: &HttpRequest) -> Result<String> {
    Ok(if core_body_b64() {
        serde_json::to_string(req)?
    } else {
        serde_json::to_string(&req.legacy())?
    })
}

thread_local! {
    static CAP_SINK: std::cell::RefCell<Option<CapSink>> =
        const { std::cell::RefCell::new(None) };
    static CAP_STREAM_SINK: std::cell::RefCell<Option<CapStreamSink>> =
        const { std::cell::RefCell::new(None) };
}

static PROCESS_SINK: RwLock<Option<SharedCapSink>> = RwLock::new(None);
static PROCESS_STREAM_SINK: RwLock<Option<SharedCapStreamSink>> = RwLock::new(None);

fn swap_process<T>(slot: &RwLock<Option<T>>, value: Option<T>) -> Option<T> {
    std::mem::replace(&mut *slot.write().unwrap_or_else(|p| p.into_inner()), value)
}

fn process_slot<T: Clone>(slot: &RwLock<Option<T>>) -> Option<T> {
    slot.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Install `sink` for every thread of the process for the duration of `body`,
/// then restore the previous one (even on panic). A thread-local sink from
/// [`with_cap_sink`] still takes precedence on its own thread.
pub fn with_process_cap_sink<R>(sink: SharedCapSink, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<SharedCapSink>);
    impl Drop for Restore {
        fn drop(&mut self) {
            swap_process(&PROCESS_SINK, self.0.take());
        }
    }
    let _restore = Restore(swap_process(&PROCESS_SINK, Some(sink)));
    body()
}

/// The streaming counterpart of [`with_process_cap_sink`].
pub fn with_process_cap_stream_sink<R>(sink: SharedCapStreamSink, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<SharedCapStreamSink>);
    impl Drop for Restore {
        fn drop(&mut self) {
            swap_process(&PROCESS_STREAM_SINK, self.0.take());
        }
    }
    let _restore = Restore(swap_process(&PROCESS_STREAM_SINK, Some(sink)));
    body()
}

/// Install `sink` for the current thread, run `body`, then restore the previous
/// sink (even on panic). The serve loop wraps each `Invoke` dispatch in this.
/// Non-reentrant: a nested call replaces the sink for the inner scope.
pub fn with_cap_sink<R>(sink: CapSink, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<CapSink>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAP_SINK.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let prev = CAP_SINK.with(|c| c.borrow_mut().replace(sink));
    let _restore = Restore(prev);
    body()
}

/// Route one op through the installed capability sink. `Some(_)` in subprocess
/// mode; `None` when no sink is installed (fall through to FFI / in-core).
/// Blocks the calling thread for the round-trip; async code uses
/// [`cap_route_async`].
pub(crate) fn cap_route(cap: &str, op_json: &str) -> Option<Result<String>> {
    let local = CAP_SINK.with(|c| c.borrow_mut().as_mut().map(|sink| sink(cap, op_json)));
    local
        .or_else(|| process_slot(&PROCESS_SINK).map(|sink| sink(cap, op_json)))
        .map(|r| r.map_err(|e| anyhow!("capability {cap}: {e}")))
}

/// [`cap_route`] without blocking an async worker: the process-wide sink's
/// round-trip runs on the blocking pool. A thread-local sink is not `Send`, so it
/// runs inline.
pub(crate) async fn cap_route_async(cap: &str, op_json: String) -> Option<Result<String>> {
    if CAP_SINK.with(|c| c.borrow().is_some()) {
        return cap_route(cap, &op_json);
    }
    let sink = process_slot(&PROCESS_SINK)?;
    let name = cap.to_string();
    let reply = tokio::task::spawn_blocking(move || sink(&name, &op_json))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    Some(reply.map_err(|e| anyhow!("capability {cap}: {e}")))
}

/// Install a streaming sink for the current thread for the duration of `body`,
/// then restore the previous one (even on panic). The serve loop wraps each
/// `Invoke` dispatch in this alongside [`with_cap_sink`].
pub fn with_cap_stream_sink<R>(sink: CapStreamSink, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<CapStreamSink>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAP_STREAM_SINK.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let prev = CAP_STREAM_SINK.with(|c| c.borrow_mut().replace(sink));
    let _restore = Restore(prev);
    body()
}

/// Route one streaming op through the installed stream sink, delivering each
/// chunk to `on_chunk`. `Some(_)` in subprocess mode; `None` when no stream sink
/// is installed.
pub(crate) fn cap_route_stream(
    cap: &str,
    op_json: &str,
    on_chunk: &mut dyn FnMut(u64, String) -> std::result::Result<(), String>,
) -> Option<Result<()>> {
    let local = CAP_STREAM_SINK.with(|c| {
        c.borrow_mut()
            .as_mut()
            .map(|sink| sink(cap, op_json, &mut *on_chunk))
    });
    local
        .or_else(|| process_slot(&PROCESS_STREAM_SINK).map(|sink| sink(cap, op_json, on_chunk)))
        .map(|r| r.map_err(|e| anyhow!("capability {cap}: {e}")))
}

/// Perform an HTTP request through orca's runtime instead of a plugin-local
/// client. In subprocess mode this routes over the capability sink as an
/// `http.request` round-trip, so the plugin links **no** reqwest/rustls/hyper —
/// the single largest source of plugin bloat. The daemon executes it on its one
/// HTTP/TLS stack and relays the response for any status.
///
/// Errors if no capability sink is installed (i.e. not running as an orca
/// subprocess): an in-process cdylib still uses its own linked HTTP client, and
/// the delegated path is only meaningful when orca is on the other end of the
/// socket. This is the seam the delegated-HTTP shim (and Phase B'd progenitor
/// clients) execute through.
pub fn http_request(req: &HttpRequest) -> Result<HttpResponse> {
    http_reply(cap_route("http.request", &http_op(req)?))
}

/// [`http_request`] for async code: awaits the round-trip instead of blocking
/// the worker it runs on.
pub async fn http_request_async(req: &HttpRequest) -> Result<HttpResponse> {
    http_reply(cap_route_async("http.request", http_op(req)?).await)
}

fn http_op(req: &HttpRequest) -> Result<String> {
    if req.follow_redirects == Some(false) && !core_no_follow() {
        return Err(anyhow!(
            "http.request: follow_redirects=false needs a core advertising {}; upgrade orca",
            crate::abi::HTTP_NO_FOLLOW
        ));
    }
    request_wire(req)
}

fn http_reply(routed: Option<Result<String>>) -> Result<HttpResponse> {
    match routed {
        Some(reply_json) => Ok(serde_json::from_str(&reply_json?)?),
        None => Err(anyhow!(
            "http.request capability unavailable: this plugin is not running as an orca subprocess"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sink_falls_through() {
        assert!(cap_route("db.op", "{}").is_none());
    }

    #[test]
    fn sink_routes_within_scope_and_restores_after() {
        let out = with_cap_sink(
            Box::new(|cap: &str, json: &str| Ok(format!("reply:{cap}:{json}"))),
            || cap_route("secret.op", "{\"x\":1}"),
        );
        assert_eq!(out.unwrap().unwrap(), "reply:secret.op:{\"x\":1}");
        assert!(cap_route("secret.op", "{}").is_none());
    }

    #[test]
    fn sink_error_maps_to_anyhow() {
        let r = with_cap_sink(Box::new(|_: &str, _: &str| Err("boom".to_string())), || {
            cap_route("db.op", "{}")
        });
        let err = r.unwrap().unwrap_err().to_string();
        assert!(err.contains("db.op") && err.contains("boom"), "got: {err}");
    }

    #[test]
    fn http_request_routes_through_sink() {
        let req = HttpRequest {
            method: "GET".into(),
            url: "https://example.test/x".into(),
            headers: vec![("accept".into(), "application/json".into())],
            body: Vec::new(),
            timeout_ms: None,
            insecure: false,
            max_body_bytes: None,
            body_b64: true,
            follow_redirects: None,
            connect_timeout_ms: None,
            retry: None,
        };
        let out = with_cap_sink(
            Box::new(|cap: &str, json: &str| {
                assert_eq!(cap, "http.request");
                assert!(json.contains("example.test"));
                Ok(r#"{"status":204,"headers":[],"body":[]}"#.to_string())
            }),
            || http_request(&req),
        );
        assert_eq!(out.unwrap().status, 204);
    }

    fn post(body: &[u8]) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            url: "https://example.test/x".into(),
            headers: vec![],
            body: body.to_vec(),
            timeout_ms: None,
            insecure: false,
            max_body_bytes: None,
            body_b64: true,
            follow_redirects: None,
            connect_timeout_ms: None,
            retry: None,
        }
    }

    #[test]
    #[allow(clippy::disallowed_types)] // inspects the raw wire JSON
    fn request_body_encoding_follows_core_support() {
        set_core_body_b64(false);
        let old: serde_json::Value =
            serde_json::from_str(&request_wire(&post(b"hi")).unwrap()).unwrap();
        assert_eq!(old["body"], serde_json::json!([104, 105]));
        assert_eq!(old["body_b64"], serde_json::json!(true));
        set_core_body_b64(true);
        let new: serde_json::Value =
            serde_json::from_str(&request_wire(&post(b"hi")).unwrap()).unwrap();
        set_core_body_b64(false);
        assert_eq!(new["body"], serde_json::json!("aGk="));
    }

    #[test]
    fn http_request_accepts_legacy_and_base64_replies() {
        for reply in [
            r#"{"status":200,"headers":[],"body":[104,105]}"#,
            r#"{"status":200,"headers":[],"body":"aGk="}"#,
        ] {
            let out = with_cap_sink(
                Box::new(move |_: &str, _: &str| Ok(reply.to_string())),
                || http_request(&post(b"")),
            );
            assert_eq!(out.unwrap().body, b"hi");
        }
    }

    #[test]
    fn http_request_without_sink_errors() {
        let req = HttpRequest {
            method: "GET".into(),
            url: "https://example.test".into(),
            headers: vec![],
            body: vec![],
            timeout_ms: None,
            insecure: false,
            max_body_bytes: None,
            body_b64: true,
            follow_redirects: None,
            connect_timeout_ms: None,
            retry: None,
        };
        let err = http_request(&req).unwrap_err().to_string();
        assert!(
            err.contains("not running as an orca subprocess"),
            "got: {err}"
        );
    }

    #[test]
    fn http_request_async_routes_through_sink() {
        let out = with_cap_sink(
            Box::new(|cap: &str, _: &str| {
                assert_eq!(cap, "http.request");
                Ok(r#"{"status":201,"headers":[],"body":[]}"#.to_string())
            }),
            || crate::reactor::block_on(http_request_async(&post(b""))),
        );
        assert_eq!(out.unwrap().status, 201);
    }

    #[test]
    fn privileged_route_needs_the_advertised_cap_and_a_sink() {
        let sink = || -> CapSink {
            Box::new(|cap: &str, _: &str| {
                assert_eq!(cap, crate::abi::LXC_EXEC);
                Ok(r#"{"success":true,"code":0,"stdout":"{}","stderr":""}"#.into())
            })
        };
        set_core_privileged(&[]);
        assert!(with_cap_sink(sink(), || privileged_route(crate::abi::LXC_EXEC, "{}")).is_none());
        set_core_privileged(&[crate::abi::LXC_EXEC.to_string()]);
        assert!(privileged_route(crate::abi::LXC_EXEC, "{}").is_none());
        assert!(
            with_cap_sink(sink(), || privileged_route(
                crate::abi::PRIVILEGED_APPLY,
                "{}"
            ))
            .is_none()
        );
        let out = with_cap_sink(sink(), || privileged_route(crate::abi::LXC_EXEC, "{}"));
        set_core_privileged(&[]);
        assert_eq!(out.unwrap().unwrap().code, Some(0));
    }

    #[test]
    fn core_advertises_reflects_welcome_caps() {
        set_core_privileged(&[]);
        assert!(!core_advertises(crate::abi::LXC_PUSH));
        set_core_privileged(&[crate::abi::LXC_PUSH.to_string()]);
        assert!(core_advertises(crate::abi::LXC_PUSH));
        assert!(!core_advertises(crate::abi::LXC_EXEC));
        set_core_privileged(&[]);
    }

    #[test]
    fn no_follow_requires_core_capability() {
        let req = HttpRequest {
            follow_redirects: Some(false),
            connect_timeout_ms: None,
            retry: None,
            ..post(b"")
        };
        let reply = || {
            with_cap_sink(
                Box::new(|_: &str, _: &str| Ok(r#"{"status":302,"headers":[],"body":[]}"#.into())),
                || http_request(&req),
            )
        };
        set_core_no_follow(false);
        let err = reply().unwrap_err().to_string();
        assert!(err.contains(crate::abi::HTTP_NO_FOLLOW), "got: {err}");
        set_core_no_follow(true);
        let out = reply();
        set_core_no_follow(false);
        assert_eq!(out.unwrap().status, 302);
        let followed = with_cap_sink(
            Box::new(|_: &str, _: &str| Ok(r#"{"status":200,"headers":[],"body":[]}"#.into())),
            || http_request(&post(b"")),
        );
        assert_eq!(followed.unwrap().status, 200);
    }

    #[test]
    fn with_core_caps_enables_no_follow_in_scope_only() {
        let req = HttpRequest {
            follow_redirects: Some(false),
            ..post(b"")
        };
        let sink = || -> CapSink {
            Box::new(|_: &str, _: &str| Ok(r#"{"status":302,"headers":[],"body":[]}"#.into()))
        };
        let before = with_cap_sink(sink(), || http_request(&req));
        assert!(before.is_err());
        let out = with_core_caps(&[crate::abi::HTTP_NO_FOLLOW], || {
            with_cap_sink(sink(), || http_request(&req))
        });
        assert_eq!(out.unwrap().status, 302);
        let err = with_cap_sink(sink(), || http_request(&req))
            .unwrap_err()
            .to_string();
        assert!(err.contains(crate::abi::HTTP_NO_FOLLOW), "got: {err}");
    }

    #[test]
    fn nested_with_core_caps_restores_outer_scope() {
        use crate::abi::{HTTP_BODY_B64, HTTP_NO_FOLLOW, LXC_EXEC, TOOL_CALL};
        let no_follow = core_no_follow;
        let body_b64 = core_body_b64;
        with_core_caps(&[HTTP_NO_FOLLOW, TOOL_CALL], || {
            with_core_caps(&[HTTP_BODY_B64, LXC_EXEC], || {
                assert!(body_b64() && !no_follow());
                assert!(core_advertises(LXC_EXEC) && !core_advertises(TOOL_CALL));
            });
            assert!(no_follow() && !body_b64());
            assert!(core_advertises(TOOL_CALL) && !core_advertises(LXC_EXEC));
        });
        assert!(!no_follow() && !body_b64() && !core_advertises(TOOL_CALL));
    }
}
