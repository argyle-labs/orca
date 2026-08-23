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
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use utoipa::ToSchema;

use crate::serve::middleware::{AuthIdentity, AuthKind, SESSION_COOKIE, SESSION_TTL};

/// Runtime-set by `serve::run` so cookie attributes can vary between dev
/// (cross-port browser ↔ API: SameSite=Lax) and production.
static DEV_MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub fn set_dev_mode(dev: bool) {
    _ = DEV_MODE.set(dev);
}

/// Use Lax everywhere. Strict can silently block cookies on self-signed TLS
/// (common in homelab deployments). Lax still protects against CSRF — the
/// server is HTTPS-only and HttpOnly prevents JS access.
fn same_site() -> &'static str {
    "Lax"
}

/// No Secure attribute: the daemon's TLS cert is self-signed and several
/// browsers silently refuse to store Secure cookies from untrusted issuers.
/// The server only speaks TLS so the cookie can never be sent over HTTP anyway.
fn secure_attr() -> &'static str {
    ""
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

fn new_id() -> String {
    utils::id::new()
}

fn new_session_id() -> String {
    new_id()
}

/// Build the `Set-Cookie` header value for a freshly minted session.
fn session_cookie_value(session_id: &str) -> String {
    format!(
        "{name}={sid}; Path=/; Max-Age={ttl}; HttpOnly;{sec} SameSite={ss}",
        name = SESSION_COOKIE,
        sid = session_id,
        ttl = SESSION_TTL.as_secs(),
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

pub(crate) fn public_signup_enabled(conn: &db::Conn) -> bool {
    db::feature_flags::get(conn, "auth.public_signup_enabled")
        .ok()
        .flatten()
        .unwrap_or(false)
}

#[utoipa::path(
    get,
    path = "/api/auth/web/signup_status",
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
    let count = auth::users::count(&conn).unwrap_or(0);
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
    path = "/api/auth/web/signup",
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
pub async fn signup(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<SignupRequest>,
) -> Response {
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

    let count = auth::users::count(&conn).unwrap_or(0);
    let first_user = count == 0;
    if !first_user && !public_signup_enabled(&conn) {
        return err(
            StatusCode::FORBIDDEN,
            "public sign-up is closed; ask an admin to create your account",
        );
    }

    if auth::users::find_auth_by_username(&conn, username)
        .ok()
        .flatten()
        .is_some()
    {
        return err(StatusCode::CONFLICT, "username already taken");
    }

    let hash = match auth::password::hash_password(&req.password) {
        Ok(h) => h,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("hash: {e}")),
    };
    let user_id = new_id();
    let now = utils::time::now_rfc3339();
    let role = if first_user { "admin" } else { "member" };
    if let Err(e) = auth::users::insert(&conn, &user_id, username, &hash, role, &now) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("insert: {e}"));
    }

    issue_session(&conn, &user_id, username, role, peer.ip().is_loopback())
}

#[utoipa::path(
    post,
    path = "/api/auth/web/signin",
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
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    let row = match auth::login::verify_credentials(&conn, &ip, &req.username, &req.password) {
        Ok(auth::login::VerifyOutcome::Verified(row)) => row,
        Ok(auth::login::VerifyOutcome::Throttled { retry_after_secs }) => {
            tracing::warn!(
                ip = %ip,
                username = %req.username,
                retry_after_secs,
                "signin throttled"
            );
            return throttled_response(retry_after_secs);
        }
        Ok(auth::login::VerifyOutcome::Invalid) => {
            tracing::warn!(
                ip = %ip,
                username = %req.username,
                "signin failed: invalid credentials"
            );
            return err(StatusCode::UNAUTHORIZED, "invalid credentials");
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("lookup: {e}")),
    };
    tracing::info!(ip = %ip, username = %row.username, user_id = %row.id, "signin ok");
    issue_session(
        &conn,
        &row.id,
        &row.username,
        &row.role,
        peer.ip().is_loopback(),
    )
}

fn persist_cli_session(conn: &db::Conn, sid: &str) -> anyhow::Result<()> {
    let session_path = files::ops::orca_home()
        .map(|d| d.join("session"))
        .ok_or_else(|| anyhow::anyhow!("no ORCA_HOME/HOME"))?;
    if let Ok(prev) = std::fs::read_to_string(&session_path) {
        let prev = prev.trim();
        if !prev.is_empty() && prev != sid {
            _ = auth::sessions::revoke(conn, prev, &utils::time::now_rfc3339());
        }
    }
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent)?;
        files::ops::chmod_dir_owner_only(parent).ok();
    }
    std::fs::write(&session_path, sid)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&session_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&session_path, perms)?;
    }
    Ok(())
}

fn issue_session(
    conn: &db::Conn,
    user_id: &str,
    username: &str,
    role: &str,
    from_loopback: bool,
) -> Response {
    let sid = new_session_id();
    let now = utils::time::now();
    let exp = now.plus(SESSION_TTL);
    if let Err(e) =
        auth::sessions::insert(conn, &sid, user_id, &now.to_rfc3339(), &exp.to_rfc3339())
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("session: {e}"));
    }
    // Mirror the session to `$ORCA_HOME/session` on loopback so the CLI on
    // the same host picks it up without a second sign-in. Same single-session
    // contract as `auth.login`: revoke any prior CLI sid first. Remote
    // browsers never touch the on-disk slot.
    if from_loopback {
        if let Err(e) = persist_cli_session(conn, &sid) {
            tracing::warn!(error = %e, "cli session mirror failed (continuing)");
        } else {
            tracing::info!("mirrored session to $ORCA_HOME/session (loopback signin)");
        }
    }
    let body = Json(SessionOk {
        user_id: user_id.into(),
        username: username.into(),
        role: role.into(),
    });
    let mut resp = body.into_response();
    let cookie_str = session_cookie_value(&sid);
    // Temporary: log exact Set-Cookie value so we can diagnose Firefox rejecting
    // it. Masks the session id but leaves attrs intact.
    let masked = cookie_str.replacen(&sid, "<sid>", 1);
    tracing::info!(set_cookie = %masked, "issuing session cookie");
    resp.headers_mut().insert(
        header::SET_COOKIE,
        cookie_str.parse().expect("cookie value is ascii"),
    );
    resp
}

#[utoipa::path(
    post,
    path = "/api/auth/web/signout",
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
        _ = auth::sessions::revoke(&conn, session_id, &utils::time::now_rfc3339());
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
    path = "/api/auth/web/change_password",
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

    let user = match auth::users::find_by_id(&conn, &user_id) {
        Ok(Some(u)) => u,
        _ => return err(StatusCode::UNAUTHORIZED, "user no longer exists"),
    };
    let auth = match auth::users::find_auth_by_username(&conn, &user.username) {
        Ok(Some(a)) => a,
        _ => return err(StatusCode::UNAUTHORIZED, "user no longer exists"),
    };
    let ok = auth::password::verify_password(&body.current_password, &auth.password_hash)
        .unwrap_or(false);
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "current password incorrect");
    }

    let hash = match auth::password::hash_password(&body.new_password) {
        Ok(h) => h,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("hash: {e}")),
    };
    let now = utils::time::now_rfc3339();
    if let Err(e) = auth::users::set_password_hash(&conn, &user_id, &hash, &now) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("update: {e}"));
    }
    Json(ChangePasswordOk { ok: true }).into_response()
}

#[utoipa::path(
    get,
    path = "/api/auth/web/me",
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
                AuthKind::Token { id, name, .. } => (id.clone(), name.clone()),
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
    fn new_id_is_valid() {
        let id = new_id();
        // Canonical form: 36 chars, 8-4-4-4-12 hyphenated hex. The
        // time-ordering guarantee is covered by `utils::id`'s own tests.
        assert_eq!(id.len(), 36, "id={id}");
        assert!(utils::id::is_valid(&id), "must be a valid id: {id}");
    }

    #[test]
    fn new_session_id_is_valid() {
        let sid = new_session_id();
        assert_eq!(sid.len(), 36, "sid={sid}");
        assert!(utils::id::is_valid(&sid), "must be a valid id: {sid}");
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
    fn same_site_returns_lax() {
        assert_eq!(same_site(), "Lax");
    }

    #[test]
    fn secure_attr_is_empty() {
        assert_eq!(secure_attr(), "");
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
        // After the first call wins, same_site/secure_attr are always fixed now
        assert_eq!(same_site(), "Lax");
        assert_eq!(secure_attr(), "");
    }

    fn test_conn() -> (tempfile::TempDir, db::Conn) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = db::open_unencrypted(&dir.path().join("orca.db")).expect("open_unencrypted");
        (dir, conn)
    }

    #[test]
    fn public_signup_enabled_defaults_false() {
        let (_d, conn) = test_conn();
        assert!(!public_signup_enabled(&conn));
    }

    #[test]
    fn public_signup_enabled_reflects_feature_flag() {
        let (_d, conn) = test_conn();
        db::feature_flags::set(&conn, "auth.public_signup_enabled", true).unwrap();
        assert!(public_signup_enabled(&conn));
        db::feature_flags::set(&conn, "auth.public_signup_enabled", false).unwrap();
        assert!(!public_signup_enabled(&conn));
    }

    // ---- serde shapes -----------------------------------------------------

    #[test]
    fn signup_request_deserializes_from_json() {
        let req: SignupRequest =
            serde_json::from_str(r#"{"username":"alice","password":"hunter2!"}"#).unwrap();
        assert_eq!(req.username, "alice");
        assert_eq!(req.password, "hunter2!");
    }

    #[test]
    fn signin_request_deserializes_from_json() {
        let req: SigninRequest =
            serde_json::from_str(r#"{"username":"bob","password":"secret12"}"#).unwrap();
        assert_eq!(req.username, "bob");
        assert_eq!(req.password, "secret12");
    }

    #[test]
    fn change_password_request_deserializes_from_json() {
        let req: ChangePasswordRequest =
            serde_json::from_str(r#"{"current_password":"old","new_password":"newnewnew"}"#)
                .unwrap();
        assert_eq!(req.current_password, "old");
        assert_eq!(req.new_password, "newnewnew");
    }

    #[test]
    fn change_password_request_missing_field_fails() {
        let r: Result<ChangePasswordRequest, _> =
            serde_json::from_str(r#"{"current_password":"old"}"#);
        assert!(r.is_err(), "missing new_password must fail to deserialize");
    }

    #[test]
    fn signup_request_missing_field_fails() {
        let r: Result<SignupRequest, _> = serde_json::from_str(r#"{"username":"alice"}"#);
        assert!(r.is_err(), "missing password must fail to deserialize");
    }

    #[test]
    fn session_ok_serializes_expected_shape() {
        let s = SessionOk {
            user_id: "u1".into(),
            username: "alice".into(),
            role: "admin".into(),
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"user_id":"u1","username":"alice","role":"admin"}"#
        );
    }

    #[test]
    fn me_ok_serializes_expected_shape() {
        let m = MeOk {
            user_id: "u1".into(),
            username: "bob".into(),
            role: "member".into(),
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"user_id":"u1","username":"bob","role":"member"}"#
        );
    }

    #[test]
    fn signup_status_serializes_expected_shape() {
        let s = SignupStatus {
            allowed: true,
            reason: "first_user".into(),
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"allowed":true,"reason":"first_user"}"#
        );
    }

    #[test]
    fn change_password_ok_serializes_expected_shape() {
        let ok = ChangePasswordOk { ok: true };
        assert_eq!(serde_json::to_string(&ok).unwrap(), r#"{"ok":true}"#);
    }

    #[test]
    fn auth_error_response_serializes_expected_shape() {
        let e = AuthErrorResponse {
            error: "nope".into(),
        };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"error":"nope"}"#);
    }

    // ---- err() + cookie helpers -------------------------------------------

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("read body");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    #[test]
    fn err_sets_status_and_json_error_body() {
        let resp = err(StatusCode::CONFLICT, "username already taken");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("application/json"), "content-type={ct}");
    }

    #[tokio::test]
    async fn err_body_is_serialized_error_object() {
        let resp = err(StatusCode::BAD_REQUEST, "bad");
        assert_eq!(body_string(resp).await, r#"{"error":"bad"}"#);
    }

    #[test]
    fn throttled_response_body_and_zero_retry() {
        // retry_after_secs = 0 still parses to a valid header value.
        let resp = throttled_response(0);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "0"
        );
    }

    #[test]
    fn session_cookie_value_ttl_matches_session_ttl() {
        let v = session_cookie_value("sid123");
        assert!(
            v.contains(&format!("Max-Age={}", SESSION_TTL.as_secs())),
            "v={v}"
        );
        assert!(v.starts_with(&format!("{SESSION_COOKIE}=sid123;")), "v={v}");
    }

    #[test]
    fn cookie_values_parse_as_header_values() {
        // Both cookie strings must be valid ASCII header values (the handlers
        // `.expect()` on this — guard against a regression in the format).
        session_cookie_value("abc")
            .parse::<header::HeaderValue>()
            .expect("session cookie parses");
        clear_cookie_value()
            .parse::<header::HeaderValue>()
            .expect("clear cookie parses");
    }

    // ---- handler pre-IO guard branches ------------------------------------

    fn loopback() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345)))
    }

    fn session_ident(user_id: &str, username: &str, role: &str) -> AuthIdentity {
        AuthIdentity {
            kind: AuthKind::Session {
                session_id: "sess-1".into(),
                user_id: user_id.into(),
                username: username.into(),
            },
            role: role.into(),
            can_mutate: false,
        }
    }

    #[tokio::test]
    async fn signup_rejects_empty_username() {
        let resp = signup(
            loopback(),
            Json(SignupRequest {
                username: "   ".into(),
                password: "longenough".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_string(resp).await, r#"{"error":"username required"}"#);
    }

    #[tokio::test]
    async fn signup_rejects_overlong_username() {
        let resp = signup(
            loopback(),
            Json(SignupRequest {
                username: "a".repeat(65),
                password: "longenough".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(resp).await,
            r#"{"error":"username too long (max 64)"}"#
        );
    }

    #[tokio::test]
    async fn signup_rejects_short_password() {
        let resp = signup(
            loopback(),
            Json(SignupRequest {
                username: "alice".into(),
                password: "short".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(resp).await,
            r#"{"error":"password must be at least 8 characters"}"#
        );
    }

    #[tokio::test]
    async fn change_password_requires_session_identity() {
        let ident = AuthIdentity {
            kind: AuthKind::Token {
                id: "t1".into(),
                name: "cli".into(),
                user_id: None,
            },
            role: "admin".into(),
            can_mutate: true,
        };
        let resp = change_password(
            axum::extract::Extension(ident),
            Json(ChangePasswordRequest {
                current_password: "whatever".into(),
                new_password: "longenough".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(resp).await, r#"{"error":"session required"}"#);
    }

    #[tokio::test]
    async fn change_password_rejects_bootstrap_identity() {
        let ident = AuthIdentity {
            kind: AuthKind::Bootstrap,
            role: "admin".into(),
            can_mutate: true,
        };
        let resp = change_password(
            axum::extract::Extension(ident),
            Json(ChangePasswordRequest {
                current_password: "whatever".into(),
                new_password: "longenough".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn change_password_rejects_short_new_password() {
        let resp = change_password(
            axum::extract::Extension(session_ident("u1", "alice", "member")),
            Json(ChangePasswordRequest {
                current_password: "currentpw".into(),
                new_password: "short".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(resp).await,
            r#"{"error":"new password must be at least 8 characters"}"#
        );
    }

    #[tokio::test]
    async fn me_returns_401_without_identity() {
        let req = Request::new(axum::body::Body::empty());
        let resp = me(req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(resp).await, r#"{"error":"not signed in"}"#);
    }

    #[tokio::test]
    async fn me_returns_session_identity() {
        let mut req = Request::new(axum::body::Body::empty());
        req.extensions_mut()
            .insert(session_ident("u1", "alice", "member"));
        let resp = me(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_string(resp).await,
            r#"{"user_id":"u1","username":"alice","role":"member"}"#
        );
    }

    #[tokio::test]
    async fn me_returns_token_identity_with_name_as_username() {
        let ident = AuthIdentity {
            kind: AuthKind::Token {
                id: "tok-id".into(),
                name: "ci-token".into(),
                user_id: None,
            },
            role: "read".into(),
            can_mutate: false,
        };
        let mut req = Request::new(axum::body::Body::empty());
        req.extensions_mut().insert(ident);
        let resp = me(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_string(resp).await,
            r#"{"user_id":"tok-id","username":"ci-token","role":"read"}"#
        );
    }

    #[tokio::test]
    async fn me_returns_bootstrap_identity() {
        let ident = AuthIdentity {
            kind: AuthKind::Bootstrap,
            role: "admin".into(),
            can_mutate: true,
        };
        let mut req = Request::new(axum::body::Body::empty());
        req.extensions_mut().insert(ident);
        let resp = me(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_string(resp).await,
            r#"{"user_id":"bootstrap","username":"bootstrap","role":"admin"}"#
        );
    }

    #[tokio::test]
    async fn signout_without_identity_clears_cookie_and_returns_ok() {
        // No AuthIdentity extension => skips server-side revoke (no DB) and
        // still emits the clear-cookie header with a 200.
        let req = Request::new(axum::body::Body::empty());
        let resp = signout(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(set_cookie.contains("Max-Age=0"), "set_cookie={set_cookie}");
        assert!(
            set_cookie.starts_with(&format!("{SESSION_COOKIE}=;")),
            "set_cookie={set_cookie}"
        );
    }
}
