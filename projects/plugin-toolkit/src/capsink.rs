//! Out-of-process capability sink — the plugin end of the capability channel.
//!
//! In the subprocess model there is no `set_host` FFI. Instead the toolkit's
//! serve loop installs a **capability sink** — a closure over the session socket
//! — for the duration of each `Invoke`, and delegated operations
//! (`db.op` / `secret.op` / `http.request`) route their typed payload through it
//! as a capability round-trip to orca.
//!
//! Thread-local because a plugin's serve loop and its dispatch run on ONE thread,
//! serially: the sink is valid exactly while a tool is executing, so tool code
//! deep in the call graph reaches the socket without threading a channel through
//! every signature. When no sink is installed (in-process cdylib, or in-core
//! `endpoint_resource!`) callers fall through to their FFI / pooled paths.
//!
//! This module is dependency-light on purpose (serde_json + the ABI types only,
//! no rusqlite): the delegated-HTTP shim reaches [`http_request`] without pulling
//! the `db` feature.

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

thread_local! {
    static CORE_BODY_B64: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CORE_NO_FOLLOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CORE_PRIVILEGED: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(any(test, all(feature = "tools", feature = "endpoints")))]
pub(crate) fn set_core_privileged(caps: &[String]) {
    let privileged = [
        crate::abi::LXC_EXEC,
        crate::abi::LXC_PUSH,
        crate::abi::PRIVILEGED_APPLY,
        crate::abi::TOOL_CALL,
    ];
    CORE_PRIVILEGED.with(|c| {
        *c.borrow_mut() = caps
            .iter()
            .filter(|cap| privileged.contains(&cap.as_str()))
            .cloned()
            .collect()
    });
}

pub fn core_advertises(cap: &str) -> bool {
    CORE_PRIVILEGED.with(|c| c.borrow().iter().any(|c| c == cap))
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

#[cfg(any(test, all(feature = "tools", feature = "endpoints")))]
pub(crate) fn set_core_body_b64(on: bool) {
    CORE_BODY_B64.with(|c| c.set(on));
}

#[cfg(any(test, all(feature = "tools", feature = "endpoints")))]
pub(crate) fn set_core_no_follow(on: bool) {
    CORE_NO_FOLLOW.with(|c| c.set(on));
}

fn request_wire(req: &HttpRequest) -> Result<String> {
    Ok(if CORE_BODY_B64.with(|c| c.get()) {
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
pub(crate) fn cap_route(cap: &str, op_json: &str) -> Option<Result<String>> {
    CAP_SINK.with(|c| {
        c.borrow_mut()
            .as_mut()
            .map(|sink| sink(cap, op_json).map_err(|e| anyhow!("capability {cap}: {e}")))
    })
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
    CAP_STREAM_SINK.with(|c| {
        c.borrow_mut()
            .as_mut()
            .map(|sink| sink(cap, op_json, on_chunk).map_err(|e| anyhow!("capability {cap}: {e}")))
    })
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
    if req.follow_redirects == Some(false) && !CORE_NO_FOLLOW.with(|c| c.get()) {
        return Err(anyhow!(
            "http.request: follow_redirects=false needs a core advertising {}; upgrade orca",
            crate::abi::HTTP_NO_FOLLOW
        ));
    }
    match cap_route("http.request", &request_wire(req)?) {
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
        };
        let err = http_request(&req).unwrap_err().to_string();
        assert!(
            err.contains("not running as an orca subprocess"),
            "got: {err}"
        );
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
}
