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

thread_local! {
    static CAP_SINK: std::cell::RefCell<Option<CapSink>> =
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
    match cap_route("http.request", &serde_json::to_string(req)?) {
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

    #[test]
    fn http_request_without_sink_errors() {
        let req = HttpRequest {
            method: "GET".into(),
            url: "https://example.test".into(),
            headers: vec![],
            body: vec![],
            timeout_ms: None,
            insecure: false,
        };
        let err = http_request(&req).unwrap_err().to_string();
        assert!(
            err.contains("not running as an orca subprocess"),
            "got: {err}"
        );
    }
}
