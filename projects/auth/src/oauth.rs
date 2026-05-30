use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use contract::config::APP_NAME;
// rand 0.10: fill_bytes is on the `Rng` trait (was on `RngCore`).
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

// ── DB token helpers ──────────────────────────────────────────────────────────

fn open_db() -> anyhow::Result<rusqlite::Connection> {
    db::open_default()
}

fn store_oauth(service: &str, access_token: &str, refresh_token: Option<&str>) -> Result<()> {
    let conn = open_db()?;
    db::oauth::upsert(
        &conn,
        &db::oauth::TokenRow {
            service: service.to_string(),
            access_token: access_token.to_string(),
            refresh_token: refresh_token.map(str::to_string),
            expires_at: None,
        },
    )?;
    Ok(())
}

fn load_oauth(service: &str) -> Option<db::oauth::TokenRow> {
    open_db()
        .ok()
        .and_then(|conn| db::oauth::get(&conn, service).ok().flatten())
}

fn delete_oauth(service: &str) {
    if let Ok(conn) = open_db() {
        _ = db::oauth::delete(&conn, service);
    }
}

/// Drop a stored OAuth token without printing. Used by the unified
/// `AuthService::logout` impl. Returns `true` if a row was removed.
pub fn delete_oauth_silent(service: &str) -> bool {
    let Ok(conn) = open_db() else {
        return false;
    };
    db::oauth::delete(&conn, service).ok().unwrap_or(false)
}

// Public aliases used across the codebase
pub fn load_github_token() -> Option<String> {
    load_oauth("github").map(|r| r.access_token)
}

pub fn load_atlassian_access_token() -> Option<String> {
    load_oauth("atlassian").map(|r| r.access_token)
}

pub fn load_atlassian_refresh_token() -> Option<String> {
    load_oauth("atlassian").and_then(|r| r.refresh_token)
}

/// Update just the access token for Atlassian (used by server after token refresh).
pub fn update_atlassian_access_token(access_token: &str) -> Result<()> {
    let refresh = load_atlassian_refresh_token();
    store_oauth("atlassian", access_token, refresh.as_deref())
}

// ── GitHub Device Flow ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
}

pub async fn cmd_oauth_github() -> Result<()> {
    let client_id = std::env::var("GITHUB_OAUTH_CLIENT_ID")
        .context("GITHUB_OAUTH_CLIENT_ID not set — add to .env.orca.tpl and 1Password")?;

    let client = utils::http::Client::new();

    let resp: DeviceCodeResponse = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(vec![
            ("client_id".into(), client_id.clone()),
            ("scope".into(), "repo".into()),
        ])
        .send()
        .await
        .context("device code request failed")?
        .json()
        .context("failed to parse device code response")?;

    println!();
    println!("  Open:  {}", resp.verification_uri);
    println!("  Code:  {}", resp.user_code);
    println!();
    println!("Waiting for authorization...");

    open_browser(&resp.verification_uri);

    let deadline = std::time::Instant::now() + Duration::from_secs(resp.expires_in);
    let poll_interval = Duration::from_secs(resp.interval.max(5));

    loop {
        if std::time::Instant::now() > deadline {
            bail!("authorization timed out — run '{APP_NAME} login github' again");
        }
        tokio::time::sleep(poll_interval).await;

        let token_resp: DeviceTokenResponse = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(vec![
                ("client_id".into(), client_id.clone()),
                ("device_code".into(), resp.device_code.clone()),
                (
                    "grant_type".into(),
                    "urn:ietf:params:oauth:grant-type:device_code".into(),
                ),
            ])
            .send()
            .await
            .context("token poll request failed")?
            .json()
            .context("failed to parse token response")?;

        match (token_resp.access_token, token_resp.error.as_deref()) {
            (Some(token), _) => {
                store_oauth("github", &token, None)?;
                println!("GitHub token stored in orca.db.");
                return Ok(());
            }
            (_, Some("authorization_pending" | "slow_down")) => continue,
            (_, Some(err)) => bail!("authorization failed: {err}"),
            _ => continue,
        }
    }
}

pub fn cmd_logout_github() -> Result<()> {
    delete_oauth("github");
    println!("GitHub token removed from orca.db.");
    Ok(())
}

// ── Atlassian OAuth 2.0 (3LO) with PKCE ─────────────────────────────────────

const ATLASSIAN_AUTH_URL: &str = "https://auth.atlassian.com/authorize";
const ATLASSIAN_TOKEN_URL: &str = "https://auth.atlassian.com/oauth/token";
const ATLASSIAN_SCOPES: &str = "read:jira-work write:jira-work read:confluence-space.summary read:confluence-content.all offline_access";

#[derive(Deserialize)]
struct AtlassianTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

pub async fn cmd_oauth_atlassian() -> Result<()> {
    let client_id = std::env::var("ATLASSIAN_OAUTH_CLIENT_ID")
        .context("ATLASSIAN_OAUTH_CLIENT_ID not set — add to .env.orca.tpl and 1Password")?;
    let client_secret = std::env::var("ATLASSIAN_OAUTH_CLIENT_SECRET")
        .context("ATLASSIAN_OAUTH_CLIENT_SECRET not set")?;

    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind callback port")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/callback");

    let (verifier, challenge) = pkce_pair();
    let state = random_hex(16);

    let auth_url = format!(
        "{ATLASSIAN_AUTH_URL}?\
         audience=api.atlassian.com\
         &client_id={client_id}\
         &scope={scopes}\
         &redirect_uri={redirect_uri}\
         &state={state}\
         &response_type=code\
         &prompt=consent\
         &code_challenge_method=S256\
         &code_challenge={challenge}",
        scopes =
            url::form_urlencoded::byte_serialize(ATLASSIAN_SCOPES.as_bytes()).collect::<String>(),
        redirect_uri =
            url::form_urlencoded::byte_serialize(redirect_uri.as_bytes()).collect::<String>(),
    );

    println!("\nOpening browser for Atlassian authorization...");
    println!("If the browser doesn't open, visit:\n  {auth_url}\n");
    open_browser(&auth_url);

    let code = receive_callback(listener, &state)?;

    let token_resp: AtlassianTokenResponse = utils::http::Client::new()
        .post(ATLASSIAN_TOKEN_URL)
        .form(vec![
            ("grant_type".into(), "authorization_code".into()),
            ("client_id".into(), client_id.clone()),
            ("client_secret".into(), client_secret.clone()),
            ("code".into(), code.clone()),
            ("redirect_uri".into(), redirect_uri.clone()),
            ("code_verifier".into(), verifier.clone()),
        ])
        .send()
        .await
        .context("token exchange request failed")?
        .json()
        .context("failed to parse token response")?;

    store_oauth(
        "atlassian",
        &token_resp.access_token,
        token_resp.refresh_token.as_deref(),
    )?;
    println!("Atlassian tokens stored in orca.db.");
    Ok(())
}

pub fn cmd_logout_atlassian() -> Result<()> {
    delete_oauth("atlassian");
    println!("Atlassian tokens removed from orca.db.");
    Ok(())
}

// ── PKCE helpers ─────────────────────────────────────────────────────────────

fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest.as_slice());
    (verifier, challenge)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().fold(String::new(), |mut s, b| {
        _ = write!(s, "{b:02x}");
        s
    })
}

// ── Callback server ───────────────────────────────────────────────────────────

fn receive_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    listener.set_nonblocking(false)?;
    let (mut stream, _) = listener.accept().context("failed to accept callback")?;

    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).context("failed to read callback")?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let body = b"<html><body><h2>Authorized!</h2><p>You can close this tab.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    _ = stream.write_all(response.as_bytes());
    _ = stream.write_all(body);

    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            code = Some(v.to_string());
        } else if let Some(v) = pair.strip_prefix("state=") {
            state = Some(v.to_string());
        }
    }

    if state.as_deref() != Some(expected_state) {
        bail!("state mismatch — possible CSRF; try again");
    }
    code.context("no code in callback URL")
}

// ── Browser opener ────────────────────────────────────────────────────────────

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(url).spawn().ok();
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open").arg(url).spawn().ok();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("Cannot open browser automatically on this platform — visit the URL manually.");
}
