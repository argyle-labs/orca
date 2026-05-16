//! Direct axum handlers for browser auth: `/api/auth/signup`, `/signin`,
//! `/signout`, `/me`, `/signup_status`.
//!
//! These are NOT OrcaTools because they need to set `Set-Cookie` headers
//! that the fixed OrcaTool handler shape can't emit. CLI/MCP clients
//! authenticate with bearer tokens or mTLS client certs instead, so the
//! REST-only restriction here is intentional.

use axum::{
    Json,
    extract::Request,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::serve::middleware::{AuthIdentity, AuthKind, SESSION_COOKIE, SESSION_TTL};

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
    // Max-Age in seconds (30d). Secure because we only serve over HTTPS now.
    // SameSite=Strict prevents the browser from sending it on cross-site
    // requests — UI is the only intended caller.
    format!(
        "{name}={sid}; Path=/; Max-Age={ttl}; HttpOnly; Secure; SameSite=Strict",
        name = SESSION_COOKIE,
        sid = session_id,
        ttl = SESSION_TTL.num_seconds(),
    )
}

/// `Set-Cookie` value that immediately expires the session cookie. Used by
/// `/signout` so the browser drops the cookie even if the server-side row
/// is somehow already gone.
fn clear_cookie_value() -> String {
    format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
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
pub async fn signin(Json(req): Json<SigninRequest>) -> Response {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    let row = match db::users::find_auth_by_username(&conn, &req.username) {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::UNAUTHORIZED, "invalid credentials"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("lookup: {e}")),
    };
    let ok =
        crate::auth_password::verify_password(&req.password, &row.password_hash).unwrap_or(false);
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "invalid credentials");
    }
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
