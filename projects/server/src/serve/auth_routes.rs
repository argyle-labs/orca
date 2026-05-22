//! Direct axum handlers for browser auth: `/api/auth/signup`, `/signin`,
//! `/signout`, `/me`, `/signup_status`.
//!
//! These are NOT OrcaTools because they need to set `Set-Cookie` headers
//! that the fixed OrcaTool handler shape can't emit. CLI/MCP clients
//! authenticate with bearer tokens or mTLS client certs instead, so the
//! REST-only restriction here is intentional.

use axum::{
    Json,
    extract::{ConnectInfo, Request},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use utoipa::ToSchema;

use crate::serve::middleware::{AuthIdentity, AuthKind, SESSION_COOKIE, SESSION_TTL};

/// Runtime-set by `serve::run` so cookie attributes can vary between dev
/// (cross-port browser ↔ API: needs `SameSite=None`) and production (single
/// HTTPS origin: tighter `SameSite=Strict`).
static DEV_MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub fn set_dev_mode(dev: bool) {
    let _ = DEV_MODE.set(dev);
}

fn same_site() -> &'static str {
    if *DEV_MODE.get().unwrap_or(&false) {
        "Lax"
    } else {
        "Strict"
    }
}

/// In prod we require HTTPS so cookies get `Secure`. In dev we serve plain
/// HTTP so the attribute would prevent the cookie from being stored at all.
fn secure_attr() -> &'static str {
    if *DEV_MODE.get().unwrap_or(&false) {
        ""
    } else {
        " Secure;"
    }
}

#[derive(Deserialize, ToSchema)]
pub struct SignupRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct SigninRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Serialize, ToSchema)]
pub struct ChangePasswordOk {
    pub ok: bool,
}

#[derive(Serialize, ToSchema)]
pub struct SessionOk {
    pub user_id: String,
    pub username: String,
    pub role: String,
}

#[derive(Serialize, ToSchema)]
pub struct SignupStatus {
    pub allowed: bool,
    pub reason: String,
}

#[derive(Serialize, ToSchema)]
pub struct MeOk {
    pub user_id: String,
    pub username: String,
    pub role: String,
}

#[derive(Serialize, ToSchema)]
pub struct AuthErrorResponse {
    pub error: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(ErrorBody { error: msg.into() })).into_response()
}

fn ulid_like(prefix: &str) -> String {
    let mut buf = [0u8; 12];
    rand::rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(prefix.len() + buf.len() * 2 + 1);
    s.push_str(prefix);
    s.push('_');
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn new_session_id() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(64);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Build the `Set-Cookie` header value for a freshly minted session.
fn session_cookie_value(session_id: &str) -> String {
    format!(
        "{name}={sid}; Path=/; Max-Age={ttl}; HttpOnly;{sec} SameSite={ss}",
        name = SESSION_COOKIE,
        sid = session_id,
        ttl = SESSION_TTL.num_seconds(),
        sec = secure_attr(),
        ss = same_site(),
    )
}

/// `Set-Cookie` value that immediately expires the session cookie. Used by
/// `/signout` so the browser drops the cookie even if the server-side row
/// is somehow already gone.
fn clear_cookie_value() -> String {
    format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly;{sec} SameSite={ss}",
        sec = secure_attr(),
        ss = same_site()
    )
}

/// Build the 429 response with `Retry-After` header for throttled signins.
pub(crate) fn throttled_response(retry_after_secs: u64) -> Response {
    let mut resp = err(StatusCode::TOO_MANY_REQUESTS, "too many signin attempts");
    if let Ok(v) = retry_after_secs.to_string().parse() {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    resp
}

fn public_signup_enabled(conn: &db::Conn) -> bool {
    db::settings::secret_get(conn, "auth.public_signup_enabled")
        .ok()
        .flatten()
        .as_deref()
        .map(|v| matches!(v, "1" | "true" | "yes"))
        .unwrap_or(false)
}

#[utoipa::path(
    get,
    path = "/api/auth/signup_status",
    operation_id = "authSignupStatus",
    responses(
        (status = 200, description = "Whether sign-up is currently allowed", body = SignupStatus),
    ),
    tag = "auth"
)]
pub async fn signup_status() -> Response {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    let count = db::users::count(&conn).unwrap_or(0);
    if count == 0 {
        return Json(SignupStatus {
            allowed: true,
            reason: "first_user".into(),
        })
        .into_response();
    }
    if public_signup_enabled(&conn) {
        return Json(SignupStatus {
            allowed: true,
            reason: "public_signup_enabled".into(),
        })
        .into_response();
    }
    Json(SignupStatus {
        allowed: false,
        reason: "closed".into(),
    })
    .into_response()
}

#[utoipa::path(
    post,
    path = "/api/auth/signup",
    operation_id = "authSignup",
    request_body = SignupRequest,
    responses(
        (status = 200, description = "Account created; session cookie set", body = SessionOk),
        (status = 400, description = "Bad request (validation)", body = AuthErrorResponse),
        (status = 403, description = "Public sign-up disabled", body = AuthErrorResponse),
        (status = 409, description = "Username already taken", body = AuthErrorResponse),
    ),
    tag = "auth"
)]
pub async fn signup(Json(req): Json<SignupRequest>) -> Response {
    let username = req.username.trim();
    if username.is_empty() {
        return err(StatusCode::BAD_REQUEST, "username required");
    }
    if username.len() > 64 {
        return err(StatusCode::BAD_REQUEST, "username too long (max 64)");
    }
    if req.password.len() < 8 {
        return err(
            StatusCode::BAD_REQUEST,
            "password must be at least 8 characters",
        );
    }

    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };

    let count = db::users::count(&conn).unwrap_or(0);
    let first_user = count == 0;
    if !first_user && !public_signup_enabled(&conn) {
        return err(
            StatusCode::FORBIDDEN,
            "public sign-up is closed; ask an admin to create your account",
        );
    }

    if db::users::find_auth_by_username(&conn, username)
        .ok()
        .flatten()
        .is_some()
    {
        return err(StatusCode::CONFLICT, "username already taken");
    }

    let hash = match crate::auth_password::hash_password(&req.password) {
        Ok(h) => h,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("hash: {e}")),
    };
    let user_id = ulid_like("usr");
    let now = chrono::Utc::now().to_rfc3339();
    let role = if first_user { "admin" } else { "member" };
    if let Err(e) = db::users::insert(&conn, &user_id, username, &hash, role, &now) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("insert: {e}"));
    }

    issue_session(&conn, &user_id, username, role)
}

#[utoipa::path(
    post,
    path = "/api/auth/signin",
    operation_id = "authSignin",
    request_body = SigninRequest,
    responses(
        (status = 200, description = "Signed in; session cookie set", body = SessionOk),
        (status = 401, description = "Invalid credentials", body = AuthErrorResponse),
    ),
    tag = "auth"
)]
pub async fn signin(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<SigninRequest>,
) -> Response {
    let ip = peer.ip().to_string();
    if let crate::auth_throttle::CheckOutcome::Throttled { retry_after_secs } =
        crate::auth_throttle::check(&ip, &req.username)
    {
        tracing::warn!(
            ip = %ip,
            username = %req.username,
            retry_after_secs,
            "signin throttled"
        );
        return throttled_response(retry_after_secs);
    }

    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    let row = match db::users::find_auth_by_username(&conn, &req.username) {
        Ok(Some(r)) => r,
        Ok(None) => {
            crate::auth_throttle::record_failure(&ip, &req.username);
            tracing::warn!(
                ip = %ip,
                username = %req.username,
                "signin failed: no such user"
            );
            return err(StatusCode::UNAUTHORIZED, "invalid credentials");
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("lookup: {e}")),
    };
    let ok =
        crate::auth_password::verify_password(&req.password, &row.password_hash).unwrap_or(false);
    if !ok {
        crate::auth_throttle::record_failure(&ip, &req.username);
        tracing::warn!(
            ip = %ip,
            username = %req.username,
            user_id = %row.id,
            "signin failed: password verify mismatch"
        );
        return err(StatusCode::UNAUTHORIZED, "invalid credentials");
    }
    crate::auth_throttle::record_success(&ip, &req.username);
    tracing::info!(ip = %ip, username = %row.username, user_id = %row.id, "signin ok");
    issue_session(&conn, &row.id, &row.username, &row.role)
}

fn issue_session(conn: &db::Conn, user_id: &str, username: &str, role: &str) -> Response {
    let sid = new_session_id();
    let now = chrono::Utc::now();
    let exp = now + SESSION_TTL;
    if let Err(e) = db::sessions::insert(conn, &sid, user_id, &now.to_rfc3339(), &exp.to_rfc3339())
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("session: {e}"));
    }
    let body = Json(SessionOk {
        user_id: user_id.into(),
        username: username.into(),
        role: role.into(),
    });
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie_value(&sid)
            .parse()
            .expect("cookie value is ascii"),
    );
    resp
}

#[utoipa::path(
    post,
    path = "/api/auth/signout",
    operation_id = "authSignout",
    responses(
        (status = 200, description = "Session revoked; clear-cookie sent"),
    ),
    tag = "auth"
)]
pub async fn signout(req: Request) -> Response {
    // If the request had a valid session, revoke the row server-side.
    if let Some(ident) = req.extensions().get::<AuthIdentity>()
        && let AuthKind::Session { session_id, .. } = &ident.kind
        && let Ok(conn) = db::open_default()
    {
        let _ = db::sessions::revoke(&conn, session_id, &chrono::Utc::now().to_rfc3339());
    }
    let mut resp = (
        StatusCode::OK,
        Json(ErrorBody {
            error: String::new(),
        }),
    )
        .into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        clear_cookie_value().parse().expect("cookie value is ascii"),
    );
    resp
}

#[utoipa::path(
    post,
    path = "/api/auth/change_password",
    operation_id = "authChangePassword",
    request_body = ChangePasswordRequest,
    responses(
        (status = 200, description = "Password changed", body = ChangePasswordOk),
        (status = 400, description = "Bad request (validation)", body = AuthErrorResponse),
        (status = 401, description = "Not signed in or current password wrong", body = AuthErrorResponse),
    ),
    tag = "auth"
)]
pub async fn change_password(
    axum::extract::Extension(ident): axum::extract::Extension<AuthIdentity>,
    Json(body): Json<ChangePasswordRequest>,
) -> Response {
    let user_id = match &ident.kind {
        AuthKind::Session { user_id, .. } => user_id.clone(),
        _ => return err(StatusCode::UNAUTHORIZED, "session required"),
    };

    if body.new_password.len() < 8 {
        return err(
            StatusCode::BAD_REQUEST,
            "new password must be at least 8 characters",
        );
    }

    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };

    let user = match db::users::find_by_id(&conn, &user_id) {
        Ok(Some(u)) => u,
        _ => return err(StatusCode::UNAUTHORIZED, "user no longer exists"),
    };
    let auth = match db::users::find_auth_by_username(&conn, &user.username) {
        Ok(Some(a)) => a,
        _ => return err(StatusCode::UNAUTHORIZED, "user no longer exists"),
    };
    let ok = crate::auth_password::verify_password(&body.current_password, &auth.password_hash)
        .unwrap_or(false);
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "current password incorrect");
    }

    let hash = match crate::auth_password::hash_password(&body.new_password) {
        Ok(h) => h,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("hash: {e}")),
    };
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) = db::users::set_password_hash(&conn, &user_id, &hash, &now) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("update: {e}"));
    }
    Json(ChangePasswordOk { ok: true }).into_response()
}

#[utoipa::path(
    get,
    path = "/api/auth/me",
    operation_id = "authMe",
    responses(
        (status = 200, description = "Current identity", body = MeOk),
        (status = 401, description = "Not signed in", body = AuthErrorResponse),
    ),
    tag = "auth"
)]
pub async fn me(req: Request) -> Response {
    match req.extensions().get::<AuthIdentity>() {
        Some(ident) => {
            // Pull username out of the identity kind. For non-session auth,
            // there's no "username" — return the token name or "loopback".
            let (user_id, username) = match &ident.kind {
                AuthKind::Session {
                    user_id, username, ..
                } => (user_id.clone(), username.clone()),
                AuthKind::Token { id, name } => (id.clone(), name.clone()),
                AuthKind::Bootstrap => ("bootstrap".into(), "bootstrap".into()),
            };
            Json(MeOk {
                user_id,
                username,
                role: ident.role.clone(),
            })
            .into_response()
        }
        None => err(StatusCode::UNAUTHORIZED, "not signed in"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn ulid_like_has_prefix_and_hex_suffix() {
        let id = ulid_like("usr");
        assert!(id.starts_with("usr_"), "id={id}");
        // 12 bytes → 24 hex chars
        assert_eq!(id.len(), "usr_".len() + 24, "id={id}");
        let hex_part = &id["usr_".len()..];
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "non-hex: {hex_part}"
        );
    }

    #[test]
    fn new_session_id_is_64_hex_chars() {
        let sid = new_session_id();
        assert_eq!(sid.len(), 64, "sid={sid}");
        assert!(sid.chars().all(|c| c.is_ascii_hexdigit()), "non-hex: {sid}");
    }

    #[test]
    fn session_cookie_value_contains_required_fields() {
        let v = session_cookie_value("mysessionid");
        assert!(v.contains("mysessionid"), "v={v}");
        assert!(v.contains("Path=/"), "v={v}");
        assert!(v.contains("Max-Age="), "v={v}");
        assert!(v.contains("HttpOnly"), "v={v}");
        assert!(v.contains("SameSite="), "v={v}");
    }

    #[test]
    fn clear_cookie_value_expires_immediately() {
        let v = clear_cookie_value();
        assert!(v.contains("Max-Age=0"), "v={v}");
        assert!(v.contains("HttpOnly"), "v={v}");
        assert!(v.contains("SameSite="), "v={v}");
    }

    #[test]
    fn same_site_returns_valid_value() {
        let v = same_site();
        assert!(v == "Lax" || v == "Strict", "unexpected: {v}");
    }

    #[test]
    fn secure_attr_returns_valid_value() {
        let v = secure_attr();
        assert!(v.is_empty() || v == " Secure;", "unexpected: {v}");
    }

    #[test]
    fn throttled_response_has_429_and_retry_after() {
        let resp = throttled_response(900);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry = resp.headers().get(header::RETRY_AFTER);
        assert!(retry.is_some(), "Retry-After header missing");
        assert_eq!(retry.unwrap().to_str().unwrap(), "900");
    }

    #[test]
    fn set_dev_mode_is_idempotent() {
        // Call twice — second call should be a silent no-op (OnceLock)
        set_dev_mode(false);
        set_dev_mode(true);
        // After the first call wins, same_site/secure_attr return consistent values
        let ss = same_site();
        let sa = secure_attr();
        assert!(ss == "Lax" || ss == "Strict");
        assert!(sa.is_empty() || sa == " Secure;");
    }
}
