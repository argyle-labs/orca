use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderValue, header::HeaderName},
    middleware::Next,
    response::Response,
};
use http_body_util::BodyExt;
use uuid::Uuid;

pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";

#[derive(Clone)]
pub struct CorrelationId(pub String);

/// Paths where we skip body logging — response is too large to be useful in logs.
/// Prefix-matched: any path that starts with one of these is skipped.
const SKIP_BODY_PREFIXES: &[&str] = &[
    "/api/openapi",
    "/api/specs",
];

/// Paths to skip logging entirely (no request/response log lines).
const SKIP_LOG_PREFIXES: &[&str] = &[
    "/api/health",
    "/assets/",
    "/favicon",
];

fn skip_body(path: &str) -> bool {
    SKIP_BODY_PREFIXES.iter().any(|p| path.starts_with(p))
}

fn skip_log(path: &str) -> bool {
    SKIP_LOG_PREFIXES.iter().any(|p| path.starts_with(p))
}

pub async fn log_requests(req: Request, next: Next) -> Response {
    let cid = req
        .headers()
        .get(CORRELATION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let at_trace = tracing::enabled!(tracing::Level::TRACE);
    let no_body = skip_body(&path);
    let no_log = skip_log(&path);

    let mut req = if at_trace && !no_log {
        let (parts, body) = req.into_parts();
        let bytes = collect_body(body).await;
        if !no_body {
            tracing::trace!(
                correlation_id = %cid,
                method = %method,
                path = %path,
                body = %format_body(&bytes),
                "→ request"
            );
        } else {
            tracing::trace!(
                correlation_id = %cid,
                method = %method,
                path = %path,
                "→ request"
            );
        }
        Request::from_parts(parts, Body::from(bytes))
    } else {
        if !no_log {
            tracing::info!(correlation_id = %cid, method = %method, path = %path, "→ request");
        }
        req
    };

    req.extensions_mut().insert(CorrelationId(cid.clone()));

    let response = next.run(req).await;
    let status = response.status().as_u16();

    let (mut parts, body) = response.into_parts();
    if let Ok(val) = HeaderValue::from_str(&cid) {
        parts
            .headers
            .insert(HeaderName::from_static(CORRELATION_ID_HEADER), val);
    }

    if at_trace && !no_log {
        let bytes = collect_body(body).await;
        if !no_body {
            tracing::trace!(
                correlation_id = %cid,
                status = %status,
                body = %format_body(&bytes),
                "← response"
            );
        } else {
            tracing::trace!(
                correlation_id = %cid,
                status = %status,
                "← response (body omitted)"
            );
        }
        Response::from_parts(parts, Body::from(bytes))
    } else {
        if !no_log {
            tracing::info!(correlation_id = %cid, status = %status, "← response");
        }
        Response::from_parts(parts, body)
    }
}

async fn collect_body(body: Body) -> Bytes {
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default()
}

/// Compact-encode JSON bodies for structured log fields; truncate oversized payloads.
/// Pretty-printing is intentionally avoided — multiline strings break JSON log lines.
fn format_body(bytes: &Bytes) -> String {
    const MAX_RAW: usize = 4096;
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
            let compact = serde_json::to_string(&val).unwrap_or_else(|_| s.to_string());
            if compact.len() > MAX_RAW {
                return format!("{}…[{} bytes]", &compact[..MAX_RAW], bytes.len());
            }
            return compact;
        }
        // Not JSON — truncate raw string
        if s.len() > MAX_RAW {
            return format!("{}…[{} bytes total]", &s[..MAX_RAW], bytes.len());
        }
        return s.to_string();
    }
    format!("[{} bytes binary]", bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── skip_body ─────────────────────────────────────────────────────────────

    #[test]
    fn skip_body_matches_prefix() {
        assert!(skip_body("/api/openapi/spec.json"));
        assert!(skip_body("/api/specs/rebuy"));
        assert!(!skip_body("/api/health"));
        assert!(!skip_body("/api/agents"));
    }

    #[test]
    fn skip_body_exact_prefix_not_matches_shorter() {
        assert!(!skip_body("/api/open")); // shorter than the registered prefix
        assert!(!skip_body("/api"));
    }

    // ── skip_log ──────────────────────────────────────────────────────────────

    #[test]
    fn skip_log_matches_known_prefixes() {
        assert!(skip_log("/api/health"));
        assert!(skip_log("/assets/main.js"));
        assert!(skip_log("/favicon.ico"));
    }

    #[test]
    fn skip_log_does_not_match_other_paths() {
        assert!(!skip_log("/api/agents"));
        assert!(!skip_log("/api/sessions"));
    }

    // ── format_body ───────────────────────────────────────────────────────────

    #[test]
    fn format_body_empty_bytes_returns_empty_string() {
        let bytes = Bytes::from("");
        assert_eq!(format_body(&bytes), "");
    }

    #[test]
    fn format_body_valid_json_compacts() {
        let pretty = serde_json::json!({"key": "value", "n": 42});
        let bytes = Bytes::from(serde_json::to_string_pretty(&pretty).unwrap());
        let result = format_body(&bytes);
        // compact JSON has no newlines
        assert!(!result.contains('\n'), "should be compact: {result}");
        assert!(result.contains("\"key\""), "should contain key: {result}");
    }

    #[test]
    fn format_body_non_json_text_returns_as_is() {
        let bytes = Bytes::from("plain text body");
        assert_eq!(format_body(&bytes), "plain text body");
    }

    #[test]
    fn format_body_binary_describes_size() {
        let bytes = Bytes::from(vec![0u8, 1, 2, 255, 254]);
        let result = format_body(&bytes);
        assert!(result.contains("bytes binary"), "got: {result}");
    }
}
