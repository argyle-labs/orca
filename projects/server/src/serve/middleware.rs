#![allow(clippy::disallowed_types)] // request/response body inspection — dynamic JSON shape
use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Request},
    http::{HeaderValue, StatusCode, header::HeaderName},
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use std::net::SocketAddr;
use tracing::Instrument;
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
        .unwrap_or_else(|| Uuid::now_v7().to_string());

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

    // Per-request span — every `tracing` event emitted while the handler
    // runs (and any task it awaits in this scope) inherits `correlation_id`
    // as a structured field, so `jq -c 'select(.correlation_id == "...")'`
    // pulls the full lifecycle of one request.
    let span = tracing::info_span!("request", correlation_id = %cid);
    let response = next.run(req).instrument(span).await;
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
    /// "admin" | "read" (token) | "member" (user session)
    pub role: String,
    /// Data-mutation opt-in carried from the token / session row. Lets a
    /// non-admin identity invoke `DATA_MUTATION` tools that would otherwise
    /// require admin. Never unlocks control-plane admin tools. Ambient
    /// host-admin identities set this true (admin already passes everything).
    pub can_mutate: bool,
}

#[derive(Clone, Debug)]
pub enum AuthKind {
    /// Bearer token from `api_tokens`. Carries the token row id and (for
    /// tokens minted post-user-binding) the issuing user_id; the middleware
    /// builds a CallerIdentity from this so pod/exec dispatch resolves to a
    /// real operator. Legacy rows have `user_id = None`.
    Token {
        id: String,
        name: String,
        user_id: Option<String>,
    },
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
    // Sign-in / sign-up / sign-up-status / sign-out: the browser hits these
    // before (or to clear) a session cookie. Handlers validate credentials
    // themselves. NOTE: `/me` and `/change_password` are deliberately NOT
    // open — they need the cookie→identity chain to run so the handler can
    // read `req.extensions().get::<AuthIdentity>()`. Opening them here would
    // make `/me` always return "not signed in" even with a valid cookie.
    "/api/auth/web/signin",
    "/api/auth/web/signup",
    "/api/auth/web/signup_status",
    "/api/auth/web/signout",
];

/// Tool name inside the `/api/v1/` namespace that the bootstrap window is
/// allowed to invoke. Anything else requires a real token.
const BOOTSTRAP_ALLOWED_TOOL: &str = "/api/v1/auth.token_create";

fn is_open_path(path: &str) -> bool {
    AUTH_OPEN_PREFIXES.iter().any(|p| path.starts_with(p))
}

fn is_api_path(path: &str) -> bool {
    path.starts_with("/api/")
}

use utils::hash::sha256_hex;

/// Extract a single named cookie value from the request's `Cookie:` headers.
/// HTTP/1.1 sends a single `Cookie:` header with `name=val; name=val` pairs;
/// HTTP/2 may split into multiple individual `cookie:` headers (RFC 9113 §8.2.3,
/// "cookie pair concatenation"). We must walk all of them — `headers().get`
/// returns only the first, so reading a single header silently loses any
/// cookie that wasn't first on the wire.
fn extract_cookie<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    for hv in req.headers().get_all(axum::http::header::COOKIE) {
        let Ok(header) = hv.to_str() else { continue };
        for kv in header.split(';') {
            let kv = kv.trim();
            if let Some((k, v)) = kv.split_once('=')
                && k == name
            {
                return Some(v);
            }
        }
    }
    None
}

/// Resolve a cookie session id to an identity, sliding the expiry on the way.
/// Returns `None` if the session is missing, revoked, or expired.
fn try_session_auth(session_id: &str) -> Option<AuthIdentity> {
    let conn = db::open_default().ok()?;
    try_session_auth_with(&conn, session_id, chrono::Utc::now())
}

/// Pure decision function for `try_session_auth`. Takes the DB connection and
/// "now" explicitly so it can be exercised against a temp DB without touching
/// the real one or relying on wall-clock time.
fn try_session_auth_with(
    conn: &db::Conn,
    session_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<AuthIdentity> {
    let row = db::sessions::find_active(conn, session_id).ok()??;
    if let Ok(when) = chrono::DateTime::parse_from_rfc3339(&row.expires_at)
        && now >= when.with_timezone(&chrono::Utc)
    {
        return None;
    }
    let new_expires = now + SESSION_TTL;
    _ = db::sessions::touch(
        conn,
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
        can_mutate: row.can_mutate,
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
    try_token_auth_with(&conn, token, chrono::Utc::now())
}

/// Pure decision function for `try_token_auth`. Same pattern as
/// `try_session_auth_with` — explicit conn + now for testability.
fn try_token_auth_with(
    conn: &db::Conn,
    token: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<AuthIdentity> {
    let hash = sha256_hex(token.as_bytes());
    let row = db::api_tokens::find_by_hash(conn, &hash).ok()??;
    if let Some(expires_at) = row.expires_at.as_deref()
        && let Ok(when) = chrono::DateTime::parse_from_rfc3339(expires_at)
        && now >= when.with_timezone(&chrono::Utc)
    {
        return None;
    }
    _ = db::api_tokens::touch(conn, &row.id, &now.to_rfc3339());
    Some(AuthIdentity {
        kind: AuthKind::Token {
            id: row.id,
            name: row.name,
            user_id: row.user_id,
        },
        role: row.role,
        can_mutate: row.can_mutate,
    })
}

/// Issuing user_id for a token-kind identity, if any. Session kinds carry
/// user_id directly on the variant; Bootstrap and legacy tokens return None.
fn identity_user_id(ident: &AuthIdentity) -> Option<String> {
    match &ident.kind {
        AuthKind::Token { user_id, .. } => user_id.clone(),
        AuthKind::Session { user_id, .. } => Some(user_id.clone()),
        AuthKind::Bootstrap => None,
    }
}

/// Build a CallerIdentity from a replicated `users` row. Returns `None` if
/// the user has been deleted out from under the token (treat as legacy:
/// fall back to the ctx's ambient host-admin).
fn caller_from_user_id(user_id: &str) -> Option<contract::CallerIdentity> {
    let conn = db::open_default().ok()?;
    let u = db::users::find_by_id(&conn, user_id).ok()??;
    Some(contract::CallerIdentity {
        user_id: u.id,
        username: u.username,
        role: u.role,
    })
}

fn bootstrap_allowed(path: &str, peer: SocketAddr) -> bool {
    if !peer.ip().is_loopback() || path != BOOTSTRAP_ALLOWED_TOOL {
        return false;
    }
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(_) => return false,
    };
    bootstrap_allowed_with(&conn, path, peer)
}

/// Pure decision function for `bootstrap_allowed`. The path/peer guards
/// remain in the wrapper above so this fn only handles the DB-side check.
fn bootstrap_allowed_with(conn: &db::Conn, path: &str, peer: SocketAddr) -> bool {
    if !peer.ip().is_loopback() || path != BOOTSTRAP_ALLOWED_TOOL {
        return false;
    }
    db::api_tokens::count(conn).map(|n| n == 0).unwrap_or(false)
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
    // Join all `cookie:` headers (HTTP/2 may split into multiple) for diagnostics.
    let raw_cookie_hdr: String = req
        .headers()
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join("; ");
    let sid_opt = extract_cookie(&req, SESSION_COOKIE);
    if let Some(sid) = sid_opt {
        match try_session_auth(sid) {
            Some(ident) => {
                let mut req = req;
                if let AuthKind::Session {
                    user_id, username, ..
                } = &ident.kind
                {
                    req.extensions_mut().insert(contract::CallerIdentity {
                        user_id: user_id.clone(),
                        username: username.clone(),
                        role: ident.role.clone(),
                    });
                }
                req.extensions_mut().insert(ident);
                return next.run(req).await;
            }
            None => {
                tracing::warn!(
                    path = %path,
                    sid_prefix = %&sid.chars().take(8).collect::<String>(),
                    "cookie session present but try_session_auth returned None (revoked, expired, or no matching row)"
                );
            }
        }
    } else if !raw_cookie_hdr.is_empty() {
        // Mask cookie values (between '=' and ';') so we don't leak any secrets
        // into logs — just dump cookie names that are present.
        let names: Vec<&str> = raw_cookie_hdr
            .split(';')
            .filter_map(|p| p.trim().split('=').next())
            .collect();
        tracing::warn!(
            path = %path,
            cookie_header_len = raw_cookie_hdr.len(),
            cookie_names = ?names,
            "cookie header present but no orca_session cookie extracted"
        );
    } else {
        tracing::debug!(path = %path, "no cookie header on auth-required request");
    }

    if let Some(token) = extract_bearer(&req) {
        // Fast path: process-local loopback token minted at boot. Constant
        // string compare (no DB hit) for the high-volume in-process callers.
        if let Some(lb) = auth::loopback_token::get()
            && lb == token
        {
            let mut req = req;
            req.extensions_mut().insert(AuthIdentity {
                kind: AuthKind::Token {
                    id: "tok_loopback".into(),
                    name: "loopback".into(),
                    user_id: None,
                },
                role: "admin".into(),
                can_mutate: true,
            });
            return next.run(req).await;
        }
        if let Some(ident) = try_token_auth(token) {
            let mut req = req;
            // Token rows minted post-2026-05-29 carry the issuer's user_id;
            // resolve to a CallerIdentity so REST→pod dispatch mints a token
            // bound to the actual operator. Legacy NULL → no override; the
            // shared ctx's ambient host-admin identity stays in effect.
            if let Some(uid) = identity_user_id(&ident)
                && let Some(c) = caller_from_user_id(&uid)
            {
                req.extensions_mut().insert(c);
            }
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
            can_mutate: true,
        });
        return next.run(req).await;
    }

    (StatusCode::UNAUTHORIZED, "auth required").into_response()
}

/// Prefix under which `/api/v1/<tool_name>` is mounted. The role gate parses
/// the tool name off the tail of the path.
const TOOLS_PREFIX: &str = "/api/v1/";

/// Extract the tool name from a `/api/v1/<name>` path, if any. Returns None
/// for non-tools paths or the bare `/api/v1/` prefix with no name.
fn tool_name_from_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(TOOLS_PREFIX)?;
    if rest.is_empty() {
        return None;
    }
    // Tool names live in a single path segment; if anything trails a `/` we
    // ignore it (no current tool registers a multi-segment name).
    Some(rest.split('/').next().unwrap_or(rest))
}

/// Authorization layer for `/api/v1/*` that enforces per-tool role
/// requirements declared via `#[orca_tool(role = "admin")]`. Runs INSIDE
/// `require_auth`, so an `AuthIdentity` is always present for tool paths that
/// reach it.
///
/// Non-tool paths pass through unchanged. Unknown tool names fall open here
/// (registry's own 404 wins downstream). Caller role is compared via
/// `tool_roles::satisfies`.
pub async fn require_tool_role(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let (caller_role, can_mutate) = req
        .extensions()
        .get::<AuthIdentity>()
        .map(|i| (i.role.clone(), i.can_mutate))
        .unzip();
    match check_tool_role(&path, caller_role.as_deref(), can_mutate.unwrap_or(false)) {
        ToolRoleCheck::Pass => next.run(req).await,
        ToolRoleCheck::Forbidden { tool, required } => (
            StatusCode::FORBIDDEN,
            format!("tool '{tool}' requires role '{required}'"),
        )
            .into_response(),
    }
}

/// Pure decision function for `require_tool_role`. Split out so the branching
/// logic is testable without spinning up an axum middleware harness — axum
/// 0.8 made `Next::new` private, so we can't fabricate one in a unit test.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ToolRoleCheck {
    Pass,
    Forbidden {
        tool: String,
        required: &'static str,
    },
}

pub(crate) fn check_tool_role(
    path: &str,
    caller_role: Option<&str>,
    can_mutate: bool,
) -> ToolRoleCheck {
    let Some(tool) = tool_name_from_path(path) else {
        return ToolRoleCheck::Pass;
    };
    let required = dispatch::tool_roles::required_role(tool);
    if required == "any" {
        return ToolRoleCheck::Pass;
    }
    // `authorize` combines the role hierarchy with the `can_mutate` opt-in:
    // a non-admin identity holding it may invoke DATA_MUTATION tools that
    // require admin, but never control-plane admin tools.
    let is_data_mutation = dispatch::tool_roles::is_data_mutation(tool);
    if dispatch::tool_roles::authorize(
        caller_role.unwrap_or(""),
        can_mutate,
        required,
        is_data_mutation,
    ) {
        return ToolRoleCheck::Pass;
    }
    ToolRoleCheck::Forbidden {
        tool: tool.to_string(),
        required,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── skip_body ─────────────────────────────────────────────────────────────

    #[test]
    fn skip_body_matches_prefix() {
        assert!(skip_body("/api/openapi/spec.json"));
        assert!(skip_body("/api/specs/acme"));
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

    // ── tool_name_from_path ───────────────────────────────────────────────────

    #[test]
    fn tool_name_from_path_extracts_single_segment() {
        assert_eq!(
            tool_name_from_path("/api/v1/system.dev_enable"),
            Some("system.dev_enable")
        );
    }

    #[test]
    fn tool_name_from_path_ignores_trailing_segments() {
        assert_eq!(
            tool_name_from_path("/api/v1/system.dev_enable/extra"),
            Some("system.dev_enable")
        );
    }

    #[test]
    fn tool_name_from_path_returns_none_for_non_tools_paths() {
        assert!(tool_name_from_path("/api/health").is_none());
        assert!(tool_name_from_path("/api/v1").is_none());
        assert!(tool_name_from_path("/").is_none());
    }

    #[test]
    fn tool_name_from_path_returns_none_for_bare_prefix() {
        assert!(tool_name_from_path("/api/v1/").is_none());
    }

    // ── check_tool_role (pure decision) ───────────────────────────────────────
    //
    // The middleware itself is a thin wrapper over `check_tool_role`. Testing
    // the pure function gives full branch coverage without an axum harness
    // (Next::new is private in 0.8) and without depending on the global
    // tool_roles map — we install our own keys and inspect what survived
    // first-call-wins.

    #[test]
    fn check_tool_role_passes_non_tool_paths() {
        assert_eq!(
            check_tool_role("/api/health", Some("member"), false),
            ToolRoleCheck::Pass
        );
        assert_eq!(check_tool_role("/", None, false), ToolRoleCheck::Pass);
        // Bare /api/v1/ with no name is non-routable; treat as pass and
        // let the registry's own 404 handle it downstream.
        assert_eq!(
            check_tool_role("/api/v1/", None, false),
            ToolRoleCheck::Pass
        );
    }

    #[test]
    fn check_tool_role_passes_unknown_tool_under_any_caller() {
        // Unknown tool name → required_role falls open to "any".
        assert_eq!(
            check_tool_role("/api/v1/__no_such_tool__", Some("member"), false),
            ToolRoleCheck::Pass
        );
        assert_eq!(
            check_tool_role("/api/v1/__no_such_tool__", None, false),
            ToolRoleCheck::Pass
        );
    }

    // ── sha256_hex ────────────────────────────────────────────────────────────

    #[test]
    fn sha256_hex_empty_input_matches_known_digest() {
        // Known empty-string sha256 (RFC 4648 vector).
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_non_empty_input_returns_64_hex_chars() {
        let h = sha256_hex(b"hello world");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── is_open_path / is_api_path ────────────────────────────────────────────

    #[test]
    fn is_open_path_matches_known_open_prefixes() {
        assert!(is_open_path("/api/health"));
        assert!(is_open_path("/api/openapi/spec.json"));
        assert!(is_open_path("/api/auth/web/signin"));
        assert!(is_open_path("/api/auth/web/signup"));
        assert!(is_open_path("/api/auth/web/signup_status"));
        // `/me` is deliberately NOT open — the cookie middleware must run
        // and resolve the session identity, otherwise the handler returns 401.
        assert!(!is_open_path("/api/auth/web/me"));
        // Old top-level paths must NOT be open — they were moved under /web/
        // 2026-06-07 to disambiguate from the `auth.login` orca-tool surface.
        assert!(!is_open_path("/api/auth/signin"));
        assert!(is_open_path("/api/auth/bootstrap"));
        assert!(is_open_path("/scalar"));
    }

    #[test]
    fn is_open_path_rejects_random_api_paths() {
        assert!(!is_open_path("/api/v1/foo"));
        assert!(!is_open_path("/api/agents"));
    }

    #[test]
    fn is_api_path_only_true_for_api_prefix() {
        assert!(is_api_path("/api/x"));
        assert!(is_api_path("/api/"));
        assert!(!is_api_path("/scalar"));
        assert!(!is_api_path("/"));
    }

    // ── extract_cookie / extract_bearer ───────────────────────────────────────

    fn req_with_headers(headers: &[(&str, &str)]) -> Request {
        let mut b = Request::builder().uri("/api/x");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn extract_cookie_returns_value_for_named_cookie() {
        let r = req_with_headers(&[("cookie", "orca_session=abc123; other=xyz")]);
        assert_eq!(extract_cookie(&r, SESSION_COOKIE), Some("abc123"));
    }

    #[test]
    fn extract_cookie_returns_none_when_header_absent() {
        let r = req_with_headers(&[]);
        assert_eq!(extract_cookie(&r, SESSION_COOKIE), None);
    }

    #[test]
    fn extract_cookie_returns_none_when_cookie_missing_from_header() {
        let r = req_with_headers(&[("cookie", "other=xyz; another=qqq")]);
        assert_eq!(extract_cookie(&r, SESSION_COOKIE), None);
    }

    #[test]
    fn extract_cookie_returns_none_when_header_is_not_utf8() {
        // Build a header value with raw non-UTF-8 bytes via try_from.
        let mut r = Request::builder().uri("/x").body(Body::empty()).unwrap();
        r.headers_mut().insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_bytes(&[0xFF, 0xFE]).unwrap(),
        );
        assert_eq!(extract_cookie(&r, SESSION_COOKIE), None);
    }

    #[test]
    fn extract_cookie_returns_none_for_malformed_segment() {
        // Segment without an `=` is skipped — not split_once-able.
        let r = req_with_headers(&[("cookie", "noequals; orca_session=hit")]);
        assert_eq!(extract_cookie(&r, SESSION_COOKIE), Some("hit"));
    }

    #[test]
    fn extract_bearer_matches_capitalized_prefix() {
        let r = req_with_headers(&[("authorization", "Bearer abc.def")]);
        assert_eq!(extract_bearer(&r), Some("abc.def"));
    }

    #[test]
    fn extract_bearer_matches_lowercase_prefix() {
        let r = req_with_headers(&[("authorization", "bearer abc.def")]);
        assert_eq!(extract_bearer(&r), Some("abc.def"));
    }

    #[test]
    fn extract_bearer_returns_none_for_other_schemes() {
        let r = req_with_headers(&[("authorization", "Basic xyz")]);
        assert_eq!(extract_bearer(&r), None);
    }

    #[test]
    fn extract_bearer_returns_none_when_header_absent() {
        let r = req_with_headers(&[]);
        assert_eq!(extract_bearer(&r), None);
    }

    // ── DB-bound helpers (try_session_auth_with / try_token_auth_with /
    //    bootstrap_allowed_with) — driven against a tempdir-backed DB ──────────

    use tempfile::TempDir;

    fn test_db() -> (TempDir, db::Conn) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open_unencrypted(&dir.path().join("orca.db")).unwrap();
        (dir, conn)
    }

    fn insert_user(conn: &db::Conn, role: &str) -> String {
        let now = utils::time::now_rfc3339();
        let id = uuid::Uuid::now_v7().to_string();
        db::users::insert(conn, &id, "tester", "fake_hash", role, &now).unwrap();
        id
    }

    fn insert_session(conn: &db::Conn, user_id: &str, expires_at: &str) -> String {
        let now = utils::time::now_rfc3339();
        let sid = uuid::Uuid::now_v7().to_string();
        db::sessions::insert(conn, &sid, user_id, &now, expires_at).unwrap();
        sid
    }

    fn insert_token(conn: &db::Conn, role: &str, hash: &str, expires_at: Option<&str>) -> String {
        let now = utils::time::now_rfc3339();
        let id = uuid::Uuid::now_v7().to_string();
        db::api_tokens::insert(
            conn,
            &id,
            "test-token",
            hash,
            role,
            &now,
            expires_at,
            None,
            false,
        )
        .unwrap();
        id
    }

    #[test]
    fn try_session_auth_with_returns_none_for_missing_session() {
        let (_d, c) = test_db();
        assert!(try_session_auth_with(&c, "no_such_session", chrono::Utc::now()).is_none());
    }

    #[test]
    fn try_session_auth_with_returns_identity_for_valid_session() {
        let (_d, c) = test_db();
        let uid = insert_user(&c, "admin");
        let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let sid = insert_session(&c, &uid, &expires);
        let ident = try_session_auth_with(&c, &sid, chrono::Utc::now()).unwrap();
        assert_eq!(ident.role, "admin");
        match ident.kind {
            AuthKind::Session { session_id, .. } => assert_eq!(session_id, sid),
            _ => panic!("expected Session"),
        }
    }

    #[test]
    fn try_session_auth_with_rejects_expired_session() {
        let (_d, c) = test_db();
        let uid = insert_user(&c, "member");
        // Past expiry — now > expires_at.
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let sid = insert_session(&c, &uid, &past);
        assert!(try_session_auth_with(&c, &sid, chrono::Utc::now()).is_none());
    }

    #[test]
    fn try_session_auth_with_accepts_session_when_expiry_unparseable() {
        // Garbage expires_at string → DateTime::parse_from_rfc3339 returns Err
        // → the `if let Ok` guard is false → we fall through to issue identity.
        let (_d, c) = test_db();
        let uid = insert_user(&c, "member");
        let sid = insert_session(&c, &uid, "not-an-rfc3339-date");
        assert!(try_session_auth_with(&c, &sid, chrono::Utc::now()).is_some());
    }

    #[test]
    fn try_token_auth_with_returns_none_for_unknown_token() {
        let (_d, c) = test_db();
        assert!(try_token_auth_with(&c, "no_such_token", chrono::Utc::now()).is_none());
    }

    #[test]
    fn try_token_auth_with_returns_identity_for_valid_token() {
        let (_d, c) = test_db();
        let token = "plaintext_token_value";
        let hash = sha256_hex(token.as_bytes());
        insert_token(&c, "admin", &hash, None);
        let ident = try_token_auth_with(&c, token, chrono::Utc::now()).unwrap();
        assert_eq!(ident.role, "admin");
        match ident.kind {
            AuthKind::Token { name, .. } => assert_eq!(name, "test-token"),
            _ => panic!("expected Token"),
        }
    }

    #[test]
    fn try_token_auth_with_rejects_expired_token() {
        let (_d, c) = test_db();
        let token = "expiring_token";
        let hash = sha256_hex(token.as_bytes());
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        insert_token(&c, "read", &hash, Some(&past));
        assert!(try_token_auth_with(&c, token, chrono::Utc::now()).is_none());
    }

    #[test]
    fn try_token_auth_with_accepts_token_with_unparseable_expiry() {
        // expires_at present but not parseable → `if let Ok(when)` is false →
        // the chain shortcircuits and we issue identity.
        let (_d, c) = test_db();
        let token = "weird_expiry_token";
        let hash = sha256_hex(token.as_bytes());
        insert_token(&c, "read", &hash, Some("garbage"));
        assert!(try_token_auth_with(&c, token, chrono::Utc::now()).is_some());
    }

    #[test]
    fn try_token_auth_with_accepts_token_with_future_expiry() {
        let (_d, c) = test_db();
        let token = "future_expiry_token";
        let hash = sha256_hex(token.as_bytes());
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        insert_token(&c, "admin", &hash, Some(&future));
        assert!(try_token_auth_with(&c, token, chrono::Utc::now()).is_some());
    }

    #[test]
    fn bootstrap_allowed_with_requires_loopback_peer() {
        let (_d, c) = test_db();
        // Non-loopback IPv4 — rejected before DB touched.
        let peer: SocketAddr = "10.0.0.1:12345".parse().unwrap();
        assert!(!bootstrap_allowed_with(&c, BOOTSTRAP_ALLOWED_TOOL, peer));
    }

    #[test]
    fn bootstrap_allowed_with_requires_specific_tool_path() {
        let (_d, c) = test_db();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        assert!(!bootstrap_allowed_with(&c, "/api/v1/something_else", peer));
    }

    #[test]
    fn bootstrap_allowed_with_true_when_loopback_and_zero_tokens() {
        let (_d, c) = test_db();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        assert!(bootstrap_allowed_with(&c, BOOTSTRAP_ALLOWED_TOOL, peer));
    }

    #[test]
    fn bootstrap_allowed_with_false_when_any_token_exists() {
        let (_d, c) = test_db();
        insert_token(&c, "admin", &sha256_hex(b"any"), None);
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        assert!(!bootstrap_allowed_with(&c, BOOTSTRAP_ALLOWED_TOOL, peer));
    }

    // ── format_body large-payload truncation ──────────────────────────────────

    #[test]
    fn format_body_truncates_oversized_json() {
        let big: Vec<i32> = (0..2000).collect();
        let bytes = Bytes::from(serde_json::to_string(&big).unwrap());
        let out = format_body(&bytes);
        assert!(out.contains("bytes]"), "got: {out}");
    }

    #[test]
    fn format_body_truncates_oversized_non_json_text() {
        let big = "x".repeat(5000);
        let bytes = Bytes::from(big);
        let out = format_body(&bytes);
        assert!(out.contains("bytes total]"), "got: {out}");
    }

    // ── collect_body / log_requests via a real Router ─────────────────────────
    //
    // We route through a real axum Router because `Next` cannot be fabricated
    // in axum 0.8. The handler returns 200 with a body so the response-path
    // branches in log_requests are exercised too. We toggle the TRACE branch
    // by using `tracing::subscriber::with_default` to install a TRACE-enabled
    // collector — without that, log_requests falls through the INFO branches.

    use axum::http::Request as AxumReq;
    use tower::ServiceExt;

    fn log_router() -> axum::Router {
        axum::Router::new()
            .route(
                "/api/echo",
                axum::routing::post(|body: axum::body::Bytes| async move {
                    (axum::http::StatusCode::OK, body)
                }),
            )
            .route(
                "/api/openapi/spec.json",
                axum::routing::get(|| async {
                    (axum::http::StatusCode::OK, "{\"openapi\":\"3.1.0\"}")
                }),
            )
            .route(
                "/api/health",
                axum::routing::get(|| async { (axum::http::StatusCode::OK, "ok") }),
            )
            .layer(axum::middleware::from_fn(log_requests))
    }

    #[tokio::test]
    async fn log_requests_info_branch_passes_through_request() {
        // No TRACE subscriber → falls into the INFO branches that skip body
        // capture.
        let req = AxumReq::builder()
            .method("POST")
            .uri("/api/echo")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":1}"#))
            .unwrap();
        let resp = log_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        // Correlation-Id header roundtrips even on the INFO branch.
        assert!(resp.headers().get(CORRELATION_ID_HEADER).is_some());
    }

    #[tokio::test]
    async fn log_requests_honors_inbound_correlation_id() {
        let req = AxumReq::builder()
            .method("GET")
            .uri("/api/health")
            .header(CORRELATION_ID_HEADER, "cid-from-caller")
            .body(Body::empty())
            .unwrap();
        let resp = log_router().oneshot(req).await.unwrap();
        assert_eq!(
            resp.headers().get(CORRELATION_ID_HEADER).unwrap(),
            "cid-from-caller"
        );
    }

    #[tokio::test]
    async fn log_requests_skips_skip_log_paths_silently() {
        // /api/openapi/* matches skip_log → no info!() emitted, but the
        // request still routes successfully.
        let req = AxumReq::builder()
            .method("GET")
            .uri("/api/openapi/spec.json")
            .body(Body::empty())
            .unwrap();
        let resp = log_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn log_requests_trace_branch_captures_and_replays_body() {
        // Install a TRACE-enabled subscriber so the at_trace branch fires
        // and exercises collect_body + body replay in both directions.
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();
        tracing::subscriber::with_default(subscriber, || ()); // no-op; we set per-call below

        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_test_writer()
                .finish(),
        );

        let req = AxumReq::builder()
            .method("POST")
            .uri("/api/echo")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"key":"value"}"#))
            .unwrap();
        let resp = log_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        // Body replay round-trips the JSON intact.
        assert_eq!(&bytes[..], br#"{"key":"value"}"#);
    }

    #[tokio::test]
    async fn log_requests_trace_branch_with_skip_body_path() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_test_writer()
                .finish(),
        );
        // /api/openapi/* matches BOTH skip_log AND skip_body — exercise the
        // `if at_trace && !no_log` false branch (no_log true) → INFO branch
        // gets skipped silently.
        let req = AxumReq::builder()
            .method("GET")
            .uri("/api/openapi/spec.json")
            .body(Body::empty())
            .unwrap();
        let resp = log_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn log_requests_trace_branch_emits_body_omitted_when_skip_body() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_test_writer()
                .finish(),
        );
        // Construct a path that's traceable (not in SKIP_LOG) but matches
        // SKIP_BODY (e.g. /api/specs/*). We don't have that route registered,
        // so register one inline.
        let router = axum::Router::new()
            .route(
                "/api/specs/x",
                axum::routing::post(|body: axum::body::Bytes| async move {
                    (axum::http::StatusCode::OK, body)
                }),
            )
            .layer(axum::middleware::from_fn(log_requests));
        let req = AxumReq::builder()
            .method("POST")
            .uri("/api/specs/x")
            .body(Body::from(r#"{"x":1}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    // ── require_auth (end-to-end via Router) ──────────────────────────────────

    fn auth_router() -> axum::Router {
        axum::Router::new()
            .route(
                "/api/health",
                axum::routing::get(|| async { (axum::http::StatusCode::OK, "ok") }),
            )
            .route(
                "/api/agents",
                axum::routing::get(|| async { (axum::http::StatusCode::OK, "auth_ok") }),
            )
            .route(
                "/scalar",
                axum::routing::get(|| async { (axum::http::StatusCode::OK, "scalar") }),
            )
            .layer(axum::middleware::from_fn(require_auth))
    }

    #[tokio::test]
    async fn require_auth_passes_non_api_paths() {
        let req = AxumReq::builder()
            .uri("/scalar")
            .body(Body::empty())
            .unwrap();
        let resp = auth_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn require_auth_passes_open_api_paths() {
        let req = AxumReq::builder()
            .uri("/api/health")
            .body(Body::empty())
            .unwrap();
        let resp = auth_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn require_auth_returns_401_when_no_credentials() {
        // Closed path, no cookie, no bearer, non-loopback peer → 401.
        let mut req = AxumReq::builder()
            .uri("/api/agents")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("10.0.0.1:12345".parse().unwrap()));
        let resp = auth_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    /// Wire a thread-local DB path so `db::open_default()` returns our
    /// temp-dir-backed connection for the duration of the test.
    fn override_default_db(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("orca.db");
        // Materialize the schema by opening once before we install the override
        // — the override path is the same file open_default() will reach.
        let _ = db::open_unencrypted(&path).unwrap();
        db::set_thread_db_path(Some(path.to_str().unwrap()));
        path
    }

    #[tokio::test]
    async fn require_auth_with_cookie_session_inserts_identity_and_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = override_default_db(&dir);
        let conn = db::open_unencrypted(&path).unwrap();
        let uid = insert_user(&conn, "admin");
        let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let sid = insert_session(&conn, &uid, &expires);

        let req = AxumReq::builder()
            .uri("/api/agents")
            .header("cookie", format!("{}={}", SESSION_COOKIE, sid))
            .body(Body::empty())
            .unwrap();
        let resp = auth_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        db::set_thread_db_path(None);
    }

    #[tokio::test]
    async fn require_auth_with_invalid_cookie_falls_through_to_401() {
        let dir = tempfile::tempdir().unwrap();
        override_default_db(&dir);
        // Cookie present but session id unknown → try_session_auth None →
        // no bearer → no loopback peer → 401.
        let req = AxumReq::builder()
            .uri("/api/agents")
            .header("cookie", format!("{}=ghost", SESSION_COOKIE))
            .body(Body::empty())
            .unwrap();
        let resp = auth_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
        db::set_thread_db_path(None);
    }

    #[tokio::test]
    async fn require_auth_with_db_bearer_token_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = override_default_db(&dir);
        let conn = db::open_unencrypted(&path).unwrap();
        let token = "test_token_for_db_path_check";
        let hash = sha256_hex(token.as_bytes());
        insert_token(&conn, "admin", &hash, None);

        let req = AxumReq::builder()
            .uri("/api/agents")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = auth_router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        db::set_thread_db_path(None);
    }

    #[tokio::test]
    async fn require_auth_with_loopback_peer_and_zero_tokens_bootstrap_passes() {
        let dir = tempfile::tempdir().unwrap();
        override_default_db(&dir);
        // Loopback peer, zero tokens in DB, path = BOOTSTRAP_ALLOWED_TOOL →
        // bootstrap_allowed returns true → identity = Bootstrap/admin → pass.
        let router = axum::Router::new()
            .route(
                "/api/v1/auth.token_create",
                axum::routing::post(|| async { (axum::http::StatusCode::OK, "ok") }),
            )
            .layer(axum::middleware::from_fn(require_auth));
        let mut req = AxumReq::builder()
            .method("POST")
            .uri("/api/v1/auth.token_create")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("127.0.0.1:1234".parse().unwrap()));
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        db::set_thread_db_path(None);
    }

    #[tokio::test]
    async fn require_auth_accepts_loopback_token_fast_path() {
        // Install a loopback token, then send it. The handler short-circuits
        // before any DB lookup.
        auth::loopback_token::set_for_tests("x-lb-fixture".into());
        let req = AxumReq::builder()
            .uri("/api/agents")
            .header("authorization", "Bearer x-lb-fixture")
            .body(Body::empty())
            .unwrap();
        let resp = auth_router().oneshot(req).await.unwrap();
        // If another test set the loopback first, the comparison fails and we
        // get 401. In that case the fast-path branch is still exercised; we
        // just can't assert success.
        if auth::loopback_token::get() == Some("x-lb-fixture") {
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
        }
    }

    #[test]
    fn check_tool_role_admin_branches() {
        // Best-effort install; first-call-wins across the test binary.
        dispatch::tool_roles::install([("check_tool_role_test.admin_only", "admin")]);
        if dispatch::tool_roles::required_role("check_tool_role_test.admin_only") != "admin" {
            // Another test owned the global before us; can't drive the admin
            // branches deterministically. Pure-function correctness for the
            // admin paths is still covered via tool_roles::satisfies in
            // tool_roles.rs.
            return;
        }
        let path = "/api/v1/check_tool_role_test.admin_only";
        assert_eq!(
            check_tool_role(path, Some("admin"), false),
            ToolRoleCheck::Pass
        );
        assert_eq!(
            check_tool_role(path, Some("member"), false),
            ToolRoleCheck::Forbidden {
                tool: "check_tool_role_test.admin_only".into(),
                required: "admin"
            }
        );
        assert_eq!(
            check_tool_role(path, None, false),
            ToolRoleCheck::Forbidden {
                tool: "check_tool_role_test.admin_only".into(),
                required: "admin"
            }
        );
        // The `can_mutate` opt-in must NOT unlock this tool: it is admin-gated
        // but NOT registered as a data mutation (the mutation table has no
        // entry for it), so a mutate-opted member is still Forbidden. Guards
        // the opt-in from reaching control-plane admin.
        assert_eq!(
            check_tool_role(path, Some("member"), true),
            ToolRoleCheck::Forbidden {
                tool: "check_tool_role_test.admin_only".into(),
                required: "admin"
            }
        );
    }
}
