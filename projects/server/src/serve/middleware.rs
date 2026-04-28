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
const SKIP_LOG_PREFIXES: &[&str] = &[];

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

/// Pretty-print JSON bodies; truncate non-JSON or oversized payloads.
fn format_body(bytes: &Bytes) -> String {
    const MAX_RAW: usize = 4096;
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
            let pretty = serde_json::to_string_pretty(&val).unwrap_or_else(|_| s.to_string());
            if pretty.len() > MAX_RAW {
                return format!("{}…\n[{} bytes total]", &pretty[..MAX_RAW], bytes.len());
            }
            return pretty;
        }
        // Not JSON — truncate raw string
        if s.len() > MAX_RAW {
            return format!("{}…[{} bytes total]", &s[..MAX_RAW], bytes.len());
        }
        return s.to_string();
    }
    format!("[{} bytes binary]", bytes.len())
}
