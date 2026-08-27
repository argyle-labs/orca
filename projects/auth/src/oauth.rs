use anyhow::{Context, Result, bail};
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
    crate::oauth_store::upsert(
        &conn,
        &crate::oauth_store::TokenRow {
            service: service.to_string(),
            access_token: access_token.to_string(),
            refresh_token: refresh_token.map(str::to_string),
            expires_at: None,
        },
    )?;
    Ok(())
}

fn load_oauth(service: &str) -> Option<crate::oauth_store::TokenRow> {
    open_db()
        .ok()
        .and_then(|conn| crate::oauth_store::get(&conn, service).ok().flatten())
}

fn delete_oauth(service: &str) {
    if let Ok(conn) = open_db() {
        _ = crate::oauth_store::delete(&conn, service);
    }
}

/// Drop a stored OAuth token without printing. Used by the unified
/// `AuthService::logout` impl. Returns `true` if a row was removed.
pub fn delete_oauth_silent(service: &str) -> bool {
    let Ok(conn) = open_db() else {
        return false;
    };
    crate::oauth_store::delete(&conn, service)
        .ok()
        .unwrap_or(false)
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
    let verifier = utils::encoding::base64url_encode(&bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = utils::encoding::base64url_encode(digest.as_slice());
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

// ── Tests ──────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    // ---- random_hex --------------------------------------------------------

    #[test]
    fn random_hex_has_two_chars_per_byte() {
        assert_eq!(random_hex(16).len(), 32);
        assert_eq!(random_hex(1).len(), 2);
    }

    #[test]
    fn random_hex_zero_is_empty() {
        assert_eq!(random_hex(0), "");
    }

    #[test]
    fn random_hex_is_lowercase_hex() {
        let s = random_hex(64);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn random_hex_is_not_constant() {
        // Extremely unlikely to collide across 32 random bytes.
        assert_ne!(random_hex(32), random_hex(32));
    }

    // ---- pkce_pair ---------------------------------------------------------

    #[test]
    fn pkce_pair_challenge_derives_from_verifier() {
        let (verifier, challenge) = pkce_pair();
        // The challenge must be the base64url(SHA-256(verifier)) per RFC 7636 S256.
        let digest = Sha256::digest(verifier.as_bytes());
        let expected = utils::encoding::base64url_encode(digest.as_slice());
        assert_eq!(challenge, expected);
    }

    #[test]
    fn pkce_pair_uses_url_safe_no_pad_alphabet() {
        let (verifier, challenge) = pkce_pair();
        for s in [&verifier, &challenge] {
            assert!(!s.is_empty());
            assert!(!s.contains('='), "no padding expected: {s}");
            assert!(
                !s.contains('+') && !s.contains('/'),
                "url-safe alphabet: {s}"
            );
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "unexpected char in {s}"
            );
        }
    }

    #[test]
    fn pkce_pair_verifier_round_trips_to_32_bytes() {
        let (verifier, _) = pkce_pair();
        let bytes = utils::encoding::base64url_decode(&verifier).unwrap();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn pkce_pair_is_not_constant() {
        assert_ne!(pkce_pair().0, pkce_pair().0);
    }

    // ---- serde response shapes --------------------------------------------

    #[test]
    fn device_code_response_deserializes() {
        let json = r#"{
            "device_code": "dc-123",
            "user_code": "WXYZ-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let r: DeviceCodeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.device_code, "dc-123");
        assert_eq!(r.user_code, "WXYZ-1234");
        assert_eq!(r.verification_uri, "https://github.com/login/device");
        assert_eq!(r.expires_in, 900);
        assert_eq!(r.interval, 5);
    }

    #[test]
    fn device_token_response_success_shape() {
        let r: DeviceTokenResponse = serde_json::from_str(r#"{"access_token":"gho_abc"}"#).unwrap();
        assert_eq!(r.access_token.as_deref(), Some("gho_abc"));
        assert_eq!(r.error, None);
    }

    #[test]
    fn device_token_response_pending_error_shape() {
        let r: DeviceTokenResponse =
            serde_json::from_str(r#"{"error":"authorization_pending"}"#).unwrap();
        assert_eq!(r.access_token, None);
        assert_eq!(r.error.as_deref(), Some("authorization_pending"));
    }

    #[test]
    fn atlassian_token_response_with_refresh() {
        let r: AtlassianTokenResponse =
            serde_json::from_str(r#"{"access_token":"at-1","refresh_token":"rt-1"}"#).unwrap();
        assert_eq!(r.access_token, "at-1");
        assert_eq!(r.refresh_token.as_deref(), Some("rt-1"));
    }

    #[test]
    fn atlassian_token_response_without_refresh() {
        let r: AtlassianTokenResponse = serde_json::from_str(r#"{"access_token":"at-2"}"#).unwrap();
        assert_eq!(r.access_token, "at-2");
        assert_eq!(r.refresh_token, None);
    }

    #[test]
    fn atlassian_token_response_missing_access_token_errors() {
        let err = serde_json::from_str::<AtlassianTokenResponse>(r#"{"refresh_token":"rt"}"#);
        assert!(err.is_err());
    }

    // ---- receive_callback --------------------------------------------------

    /// Spawn a client that sends `request_line` to the given port, drains the
    /// response, and returns. Runs on its own thread so the server-side
    /// `accept()`/`read()` can proceed on the test thread.
    fn spawn_callback_client(port: u16, request_line: String) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            stream.write_all(request_line.as_bytes()).expect("write");
            stream.flush().ok();
            // Drain the server's HTTP response so its write side does not error.
            let mut sink = Vec::new();
            _ = stream.read_to_end(&mut sink);
        })
    }

    #[test]
    fn receive_callback_returns_code_on_state_match() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = spawn_callback_client(
            port,
            "GET /callback?code=the-code&state=st8 HTTP/1.1\r\nHost: localhost\r\n\r\n".into(),
        );
        let code = receive_callback(listener, "st8").unwrap();
        assert_eq!(code, "the-code");
        client.join().unwrap();
    }

    #[test]
    fn receive_callback_rejects_state_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = spawn_callback_client(
            port,
            "GET /callback?code=x&state=wrong HTTP/1.1\r\n\r\n".into(),
        );
        let err = receive_callback(listener, "expected").unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
        client.join().unwrap();
    }

    #[test]
    fn receive_callback_errors_when_code_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client =
            spawn_callback_client(port, "GET /callback?state=only HTTP/1.1\r\n\r\n".into());
        let err = receive_callback(listener, "only").unwrap_err();
        assert!(err.to_string().contains("no code"));
        client.join().unwrap();
    }

    #[test]
    fn receive_callback_handles_state_before_code_ordering() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = spawn_callback_client(
            port,
            "GET /callback?state=ord&code=late HTTP/1.1\r\n\r\n".into(),
        );
        let code = receive_callback(listener, "ord").unwrap();
        assert_eq!(code, "late");
        client.join().unwrap();
    }

    // ---- DB token helpers --------------------------------------------------
    //
    // `store_oauth`/`load_oauth`/`delete_oauth` and the public aliases all route
    // through `open_db()` → `db::open_default()`. Pointing the thread-local DB
    // path override at an ephemeral, schema-migrated SQLite file exercises the
    // real persistence path without touching the production database.

    fn with_temp_db<R>(f: impl FnOnce() -> R) -> R {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth-test.db");
        db::with_thread_db_path(&path, f)
    }

    #[test]
    fn store_and_load_round_trips_access_and_refresh() {
        with_temp_db(|| {
            store_oauth("github", "gho_xyz", Some("refresh_1")).unwrap();
            let row = load_oauth("github").unwrap();
            assert_eq!(row.access_token, "gho_xyz");
            assert_eq!(row.refresh_token.as_deref(), Some("refresh_1"));
            assert_eq!(row.expires_at, None);
        });
    }

    #[test]
    fn load_oauth_missing_service_is_none() {
        with_temp_db(|| {
            assert!(load_oauth("nope").is_none());
        });
    }

    #[test]
    fn store_oauth_upserts_existing_service() {
        with_temp_db(|| {
            store_oauth("github", "first", None).unwrap();
            store_oauth("github", "second", Some("r2")).unwrap();
            let row = load_oauth("github").unwrap();
            assert_eq!(row.access_token, "second");
            assert_eq!(row.refresh_token.as_deref(), Some("r2"));
        });
    }

    #[test]
    fn delete_oauth_removes_row() {
        with_temp_db(|| {
            store_oauth("github", "tok", None).unwrap();
            assert!(load_oauth("github").is_some());
            delete_oauth("github");
            assert!(load_oauth("github").is_none());
        });
    }

    #[test]
    fn delete_oauth_silent_reports_removal() {
        with_temp_db(|| {
            store_oauth("atlassian", "tok", None).unwrap();
            assert!(delete_oauth_silent("atlassian"));
            // Second delete finds no row.
            assert!(!delete_oauth_silent("atlassian"));
        });
    }

    #[test]
    fn public_github_alias_reads_access_token() {
        with_temp_db(|| {
            assert_eq!(load_github_token(), None);
            store_oauth("github", "gh-tok", None).unwrap();
            assert_eq!(load_github_token().as_deref(), Some("gh-tok"));
        });
    }

    #[test]
    fn public_atlassian_aliases_read_access_and_refresh() {
        with_temp_db(|| {
            store_oauth("atlassian", "acc", Some("ref")).unwrap();
            assert_eq!(load_atlassian_access_token().as_deref(), Some("acc"));
            assert_eq!(load_atlassian_refresh_token().as_deref(), Some("ref"));
        });
    }

    #[test]
    fn atlassian_refresh_alias_none_when_no_refresh_stored() {
        with_temp_db(|| {
            store_oauth("atlassian", "acc", None).unwrap();
            assert_eq!(load_atlassian_access_token().as_deref(), Some("acc"));
            assert_eq!(load_atlassian_refresh_token(), None);
        });
    }

    #[test]
    fn update_atlassian_access_token_preserves_refresh() {
        with_temp_db(|| {
            store_oauth("atlassian", "old-access", Some("keep-me")).unwrap();
            update_atlassian_access_token("new-access").unwrap();
            let row = load_oauth("atlassian").unwrap();
            assert_eq!(row.access_token, "new-access");
            assert_eq!(row.refresh_token.as_deref(), Some("keep-me"));
        });
    }

    #[test]
    fn cmd_logout_helpers_clear_stored_tokens() {
        with_temp_db(|| {
            store_oauth("github", "g", None).unwrap();
            store_oauth("atlassian", "a", Some("r")).unwrap();
            cmd_logout_github().unwrap();
            cmd_logout_atlassian().unwrap();
            assert!(load_oauth("github").is_none());
            assert!(load_oauth("atlassian").is_none());
        });
    }
}
