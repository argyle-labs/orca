#![allow(clippy::disallowed_types)] // request/response body inspection — dynamic JSON shape
use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Request},
    http::{HeaderValue, StatusCode, header::HeaderName},
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use uuid::Uuid;

pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";

#[derive(Clone)]
pub struct CorrelationId(pub String);

/// Paths where we skip body logging — response is too large to be useful in logs.
/// Prefix-matched: any path that starts with one of these is skipped.
const SKIP_BODY_PREFIXES: &[&str] = &["/api/openapi", "/api/specs"];

/// Paths to skip logging entirely (no request/response log lines).
const SKIP_LOG_PREFIXES: &[&str] = &["/api/health", "/assets/", "/favicon"];

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

// ── Auth ────────────────────────────────────────────────────────────────────

/// Identity attached to every request that passes `require_auth`.
/// Handlers can pull it via `req.extensions().get::<AuthIdentity>()`.
#[derive(Clone, Debug)]
pub struct AuthIdentity {
    pub kind: AuthKind,
    /// "admin" | "read"
    pub role: String,
}

#[derive(Clone, Debug)]
pub enum AuthKind {
    /// Bearer token from `api_tokens`. Carries the token row id.
    Token { id: String, name: String },
    /// Browser cookie session from `sessions`. Carries the session id + user id.
    Session {
        session_id: String,
        user_id: String,
        username: String,
    },
    /// Loopback with zero tokens in DB — only `auth.token_create` is reachable.
    Bootstrap,
}

/// Name of the HTTP-only cookie carrying the web-UI session id.
pub const SESSION_COOKIE: &str = "orca_session";

/// 30 days, the sliding-expiry horizon refreshed on every authenticated request.
pub const SESSION_TTL: chrono::Duration = chrono::Duration::days(30);

/// Routes reachable without auth. Keep this list short.
const AUTH_OPEN_PREFIXES: &[&str] = &[
    "/api/health",
    "/api/openapi",
    "/scalar",
    // Bootstrap probe: the TokenGate UI hits this before any token exists to
    // decide which sign-in flow to show. Handler enforces loopback + zero-tokens
    // itself, so leaving it open in middleware is safe.
    "/api/auth/bootstrap",
    // Sign-in / sign-up / sign-up-status: the browser hits these before it
    // has a session cookie. Handlers validate credentials themselves.
    "/api/auth/signin",
    "/api/auth/signup",
    "/api/auth/signup_status",
];

/// Tool name inside the `/api/tools/` namespace that the bootstrap window is
/// allowed to invoke. Anything else requires a real token.
const BOOTSTRAP_ALLOWED_TOOL: &str = "/api/tools/auth.token_create";

fn is_open_path(path: &str) -> bool {
    AUTH_OPEN_PREFIXES.iter().any(|p| path.starts_with(p))
}

fn is_api_path(path: &str) -> bool {
    path.starts_with("/api/")
}

fn sha256_hex(input: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(input);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Extract a single named cookie value from a `Cookie:` header. Handles
/// multiple cookies separated by `; ` per RFC 6265.
fn extract_cookie<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    let header = req
        .headers()
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    for kv in header.split(';') {
        let kv = kv.trim();
        if let Some((k, v)) = kv.split_once('=')
            && k == name
        {
            return Some(v);
        }
    }
    None
}

/// Resolve a cookie session id to an identity, sliding the expiry on the way.
/// Returns `None` if the session is missing, revoked, or expired.
fn try_session_auth(session_id: &str) -> Option<AuthIdentity> {
    let conn = db::open_default().ok()?;
    let row = db::sessions::find_active(&conn, session_id).ok()??;
    let now = chrono::Utc::now();
    if let Ok(when) = chrono::DateTime::parse_from_rfc3339(&row.expires_at)
        && now >= when.with_timezone(&chrono::Utc)
    {
        return None;
    }
    // Slide: refresh last_used_at + expires_at on every authenticated request.
    let new_expires = now + SESSION_TTL;
    let _ = db::sessions::touch(
        &conn,
        &row.session_id,
        &now.to_rfc3339(),
        &new_expires.to_rfc3339(),
    );
    Some(AuthIdentity {
        kind: AuthKind::Session {
            session_id: row.session_id,
            user_id: row.user_id,
            username: row.username,
        },
        role: row.role,
    })
}

fn extract_bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| {
            req.headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("bearer "))
        })
}

fn try_token_auth(token: &str) -> Option<AuthIdentity> {
    let conn = db::open_default().ok()?;
    let hash = sha256_hex(token.as_bytes());
    let row = db::api_tokens::find_by_hash(&conn, &hash).ok()??;
    // Reject if past expires_at.
    if let Some(expires_at) = row.expires_at.as_deref()
        && let Ok(when) = chrono::DateTime::parse_from_rfc3339(expires_at)
        && chrono::Utc::now() >= when.with_timezone(&chrono::Utc)
    {
        return None;
    }
    let _ = db::api_tokens::touch(&conn, &row.id, &chrono::Utc::now().to_rfc3339());
    Some(AuthIdentity {
        kind: AuthKind::Token {
            id: row.id,
            name: row.name,
        },
        role: row.role,
    })
}

fn bootstrap_allowed(path: &str, peer: SocketAddr) -> bool {
    if !peer.ip().is_loopback() {
        return false;
    }
    if path != BOOTSTRAP_ALLOWED_TOOL {
        return false;
    }
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(_) => return false,
    };
    db::api_tokens::count(&conn)
        .map(|n| n == 0)
        .unwrap_or(false)
}

/// Auth gate for `/api/*`. Order:
///   1. Open paths (`/api/health`, `/api/openapi`) — pass through.
///   2. `Authorization: Bearer <token>` matched against `api_tokens` — pass.
///   3. Loopback + zero tokens in DB + path == `auth.token_create` — bootstrap
///      pass, identity = Bootstrap/admin. Closes as soon as any token exists.
///   4. Otherwise 401.
pub async fn require_auth(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();

    if !is_api_path(&path) || is_open_path(&path) {
        return next.run(req).await;
    }

    // Cookie session — first authenticated branch, hot path for browsers.
    if let Some(sid) = extract_cookie(&req, SESSION_COOKIE)
        && let Some(ident) = try_session_auth(sid)
    {
        let mut req = req;
        req.extensions_mut().insert(ident);
        return next.run(req).await;
    }

    if let Some(token) = extract_bearer(&req) {
        // Fast path: process-local loopback token minted at boot. Constant
        // string compare (no DB hit) for the high-volume in-process callers.
        if let Some(lb) = crate::loopback_token::get()
            && lb == token
        {
            let mut req = req;
            req.extensions_mut().insert(AuthIdentity {
                kind: AuthKind::Token {
                    id: "tok_loopback".into(),
                    name: "loopback".into(),
                },
                role: "admin".into(),
            });
            return next.run(req).await;
        }
        if let Some(ident) = try_token_auth(token) {
            let mut req = req;
            req.extensions_mut().insert(ident);
            return next.run(req).await;
        }
    }

    // Bootstrap fallback — only the very first token_create call from loopback.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    if let Some(peer) = peer
        && bootstrap_allowed(&path, peer)
    {
        let mut req = req;
        req.extensions_mut().insert(AuthIdentity {
            kind: AuthKind::Bootstrap,
            role: "admin".into(),
        });
        return next.run(req).await;
    }

    (StatusCode::UNAUTHORIZED, "auth required").into_response()
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
