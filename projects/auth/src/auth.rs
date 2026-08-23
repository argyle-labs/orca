//! Auth domain — unified surface for credential management across providers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use anyhow::bail;
use derive::orca_tool;
use rand::Rng;
use utils::hash;

const ANTHROPIC_KEY: &str = "anthropic_api_key";

// ── Shared rows ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct AuthProviderStatus {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
    /// True iff a credential is currently stored for this provider.
    pub configured: bool,
    /// Masked identifier (masked API key, account login, etc.) when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct AuthStatusReport {
    pub providers: Vec<AuthProviderStatus>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct AuthStatusArgs {}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct AuthLogoutArgs {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLogoutOutput {
    pub provider: String,
    pub removed: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct AuthLoginArgs {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
    /// Required for `provider="anthropic"`. Ignored for OAuth providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLoginOutput {
    pub provider: String,
    pub stored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// Snapshot every configured credential the host knows about (Anthropic key + OAuth tokens).
#[orca_tool(domain = "auth.session", verb = "detail")]
async fn auth_session_detail(
    _args: AuthStatusArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AuthStatusReport> {
    let conn = db::open_default()?;
    let anthropic = db::settings::secret_get(&conn, ANTHROPIC_KEY)?;
    let github = crate::oauth::load_github_token();
    let atlassian = crate::oauth::load_atlassian_access_token();
    Ok(AuthStatusReport {
        providers: vec![
            AuthProviderStatus {
                provider: "anthropic".into(),
                configured: anthropic.is_some(),
                identity: anthropic.as_deref().map(db::settings::mask_key),
            },
            AuthProviderStatus {
                provider: "github".into(),
                configured: github.is_some(),
                identity: github.as_deref().map(db::settings::mask_key),
            },
            AuthProviderStatus {
                provider: "atlassian".into(),
                configured: atlassian.is_some(),
                identity: atlassian.as_deref().map(db::settings::mask_key),
            },
        ],
    })
}

/// [MUTATES STATE] Remove a stored credential. `removed=false` if nothing was stored.
#[orca_tool(domain = "auth.session", verb = "delete")]
async fn auth_session_delete(
    args: AuthLogoutArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AuthLogoutOutput> {
    let removed = match args.provider.as_str() {
        "anthropic" => {
            let conn = db::open_default()?;
            db::settings::secret_delete(&conn, ANTHROPIC_KEY)?
        }
        "github" => crate::oauth::delete_oauth_silent("github"),
        "atlassian" => crate::oauth::delete_oauth_silent("atlassian"),
        other => bail!("unknown provider '{other}' (want: anthropic|github|atlassian)"),
    };
    Ok(AuthLogoutOutput {
        provider: args.provider,
        removed,
    })
}

/// [MUTATES STATE] Authenticate with a provider. Anthropic: pass `key`. GitHub: device-flow. Atlassian: PKCE.
#[orca_tool(domain = "auth.session", verb = "create")]
async fn auth_session_create(
    args: AuthLoginArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AuthLoginOutput> {
    let provider = args.provider.as_str();
    match provider {
        "anthropic" => {
            let key = args
                .key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("`key` is required when provider=anthropic"))?;
            let conn = db::open_default()?;
            db::settings::secret_set(&conn, ANTHROPIC_KEY, key)?;
            Ok(AuthLoginOutput {
                provider: provider.into(),
                stored: true,
                identity: Some(db::settings::mask_key(key)),
            })
        }
        "github" => {
            crate::oauth::cmd_oauth_github().await?;
            let id = crate::oauth::load_github_token()
                .as_deref()
                .map(db::settings::mask_key);
            Ok(AuthLoginOutput {
                provider: provider.into(),
                stored: id.is_some(),
                identity: id,
            })
        }
        "atlassian" => {
            crate::oauth::cmd_oauth_atlassian().await?;
            let id = crate::oauth::load_atlassian_access_token()
                .as_deref()
                .map(db::settings::mask_key);
            Ok(AuthLoginOutput {
                provider: provider.into(),
                stored: id.is_some(),
                identity: id,
            })
        }
        other => bail!("unknown provider '{other}' (want: anthropic|github|atlassian)"),
    }
}

// ── API tokens (REST/MCP bearer auth, local-host scope) ─────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ApiTokenSummary {
    pub id: String,
    pub name: String,
    /// "admin" | "read"
    pub role: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Data-mutation opt-in (see `TokenCreateArgs::can_mutate`).
    #[serde(default)]
    pub can_mutate: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct TokenCreateArgs {
    /// Human-readable label (e.g. "ci-runner", "scott-laptop"). Must be unique on this host.
    pub name: String,
    /// "admin" | "read"
    pub role: String,
    /// Days until expiry. `None` = never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_days: Option<u32>,
    /// Data-mutation opt-in. Grants a non-admin (`read`) token the ability to
    /// invoke `DATA_MUTATION` tools (writes against managed systems) that would
    /// otherwise require admin — without unlocking control-plane admin tools.
    /// Default false. Meaningless on an `admin` token (admin already passes).
    #[serde(default)]
    pub can_mutate: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenCreateOutput {
    pub id: String,
    pub name: String,
    /// Plaintext bearer token — returned exactly once. Store it now; it is
    /// unrecoverable from the DB.
    pub token: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenListArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenListOutput {
    pub tokens: Vec<ApiTokenSummary>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct TokenRevokeArgs {
    pub id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenRevokeOutput {
    pub revoked: bool,
}

/// Validate a token `role` string. Pure decision extracted from
/// [`auth_token_create`] so the accept/reject branch is unit-testable
/// without a DB.
fn validate_token_role(role: &str) -> anyhow::Result<()> {
    if !matches!(role, "admin" | "read") {
        bail!("role must be 'admin' or 'read', got '{role}'");
    }
    Ok(())
}

/// Derive `(plaintext, token_hash)` from 16 random bytes. `orca_` prefix keeps
/// tokens self-identifying in logs/secret-scanners. Pure given the byte input
/// so the format + hashing is unit-testable without an RNG.
fn mint_token(raw: &[u8]) -> (String, String) {
    let plaintext = format!("orca_{}", hash::hex_encode(raw));
    let token_hash = hash::sha256_hex(plaintext.as_bytes());
    (plaintext, token_hash)
}

/// Compute the RFC3339 expiry from a base instant and an optional day count.
/// `None` days = never expires. Extracted from [`auth_token_create`] so the
/// expiry math is testable with an injected `base` instead of `now()`.
fn compute_expires_at(base: utils::time::Timestamp, days: Option<u32>) -> Option<String> {
    days.map(|d| {
        base.plus(std::time::Duration::from_secs(d as u64 * 86_400))
            .to_rfc3339()
    })
}

/// [MUTATES STATE] Mint a new REST/MCP bearer token on THIS host. Plaintext is
/// returned exactly once and cannot be recovered from the DB. Token only
/// authenticates calls to this host's `:12000` — not to other peers.
#[orca_tool(domain = "auth.token", verb = "create")]
async fn auth_token_create(
    args: TokenCreateArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenCreateOutput> {
    validate_token_role(&args.role)?;
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let (plaintext, token_hash) = mint_token(&raw);

    let id = utils::id::new();
    let now = utils::time::now_rfc3339();
    let expires_at = compute_expires_at(utils::time::now(), args.expires_in_days);

    // Bind the new token to the authenticated operator so later bearer-auth
    // requests resolve to a real user (S4 of [[project-remote-exec-full-fix]]).
    // The bootstrap path (first token, no auth yet) has no caller → user_id
    // is NULL and that token authenticates only locally.
    let caller_user_id = ctx.caller().map(|c| c.user_id);
    let conn = db::open_default()?;
    crate::api_tokens::insert(
        &conn,
        &id,
        &args.name,
        &token_hash,
        &args.role,
        &now,
        expires_at.as_deref(),
        caller_user_id.as_deref(),
        args.can_mutate,
    )?;
    Ok(TokenCreateOutput {
        id,
        name: args.name,
        token: plaintext,
    })
}

/// List all REST/MCP bearer tokens registered on this host. Token hashes are not returned.
#[orca_tool(domain = "auth.token", verb = "list")]
async fn auth_token_list(
    args: TokenListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenListOutput> {
    let conn = db::open_default()?;
    let mut rows = crate::api_tokens::list(&conn)?;
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let tokens: Vec<ApiTokenSummary> = rows
        .into_iter()
        .map(|r| ApiTokenSummary {
            id: r.id,
            name: r.name,
            role: r.role,
            created_at: r.created_at,
            last_used_at: r.last_used_at,
            expires_at: r.expires_at,
            can_mutate: r.can_mutate,
        })
        .collect();
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(tokens, &params);
    Ok(TokenListOutput {
        tokens: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

/// [MUTATES STATE] Revoke a token by id. Returns `revoked=false` if the id wasn't found.
#[orca_tool(domain = "auth.token", verb = "delete")]
async fn auth_token_delete(
    args: TokenRevokeArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenRevokeOutput> {
    let conn = db::open_default()?;
    let revoked = crate::api_tokens::revoke(&conn, &args.id)?;
    Ok(TokenRevokeOutput { revoked })
}

// Hex / sha helpers used to live here; replaced by utils::hash::*.

// ── Operator login (CLI / MCP-stdio) ────────────────────────────────────────
//
// Replaces the implicit `first_admin` ambient-identity fallback on the CLI
// and MCP-stdio surfaces (see [[project-orca-login-local-auth]]). The
// session id is held in `$ORCA_HOME/session` (mode 0600) and the row of
// record lives in the existing `sessions` table so revoke / password-reset
// flows work uniformly. 24h sliding expiry — see `resolve_host_operator` in
// `server/src/mcp/mod.rs`, which slides on each authenticated call.
pub const CLI_SESSION_TTL_SECS: i64 = 24 * 60 * 60;

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct LoginArgs {
    /// Operator username (matches the web `users` table). Omit both
    /// `username` and `password` to launch the browser flow: the CLI opens
    /// `/signin`, you sign in, and the resulting session is mirrored back
    /// to `$ORCA_HOME/session` automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Password. Omit alongside `username` for the browser flow. When
    /// supplied directly, prefer stdin to keep credentials out of shell
    /// history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct LoginOutput {
    pub user_id: String,
    pub username: String,
    /// "admin" | "read" — whatever role the user holds in `users`.
    pub role: String,
    /// RFC3339 expiry of the on-disk session.
    pub expires_at: String,
}

/// [MUTATES STATE] Authenticate the operator on THIS host and persist a CLI
/// session at `$ORCA_HOME/session` (mode 0600). Replaces the legacy
/// `first_admin` ambient-identity fallback on CLI + MCP-stdio. Omitting
/// `username`/`password` is reserved for the CLI's browser flow; the
/// daemon body refuses missing creds so an unauth'd REST caller can't
/// trip the side effect.
#[orca_tool(domain = "auth", verb = "login", cli = manual)]
async fn auth_login(args: LoginArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<LoginOutput> {
    let username = args.username.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "username required (or run `orca auth login` with no args to use the browser flow)"
        )
    })?;
    let password = args
        .password
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("password required"))?;
    // Throttle on the CLI path too: same IP-bucket as REST signin keeps brute
    // force from sneaking in via a local invocation loop.
    let ip = "127.0.0.1";

    let conn = db::open_default()?;
    let row = match crate::login::verify_credentials(&conn, ip, username, password)? {
        crate::login::VerifyOutcome::Verified(row) => row,
        crate::login::VerifyOutcome::Throttled { retry_after_secs } => {
            bail!("signin throttled — retry in {retry_after_secs}s")
        }
        crate::login::VerifyOutcome::Invalid => bail!("invalid credentials"),
    };

    let session_path = files::ops::orca_home()
        .map(|d| d.join("session"))
        .ok_or_else(|| anyhow::anyhow!("no ORCA_HOME/HOME — cannot persist session"))?;

    // Single-session model: revoke any previous CLI session for this host.
    if let Ok(prev) = std::fs::read_to_string(&session_path) {
        let prev = prev.trim();
        if !prev.is_empty() {
            let now = utils::time::now_rfc3339();
            crate::sessions::revoke(&conn, prev, &now).ok();
        }
    }

    let mut sid_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut sid_bytes);
    let sid = hash::hex_encode(&sid_bytes);
    let now = utils::time::now();
    let exp = now.plus(std::time::Duration::from_secs(CLI_SESSION_TTL_SECS as u64));
    crate::sessions::insert(&conn, &sid, &row.id, &now.to_rfc3339(), &exp.to_rfc3339())?;

    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent)?;
        files::ops::chmod_dir_owner_only(parent).ok();
    }
    std::fs::write(&session_path, &sid)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&session_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&session_path, perms)?;
    }
    Ok(LoginOutput {
        user_id: row.id,
        username: row.username,
        role: row.role,
        expires_at: exp.to_rfc3339(),
    })
}

/// Manual CLI block for `orca auth login`. The default `register_op!` would
/// dispatch every invocation through the daemon's REST surface — but the
/// no-arg form opens the browser-driven flow, which is a purely client-side
/// affordance the daemon must not know about. So we hand-roll the CliOp:
///
/// - `--peer <host>` set → remote dispatch as usual.
/// - both `username` and `password` provided → local daemon dispatch (same
///   as the auto-generated path).
/// - neither provided → open `/signin` in the default browser and poll
///   `$ORCA_HOME/session` for the freshly-mirrored sid. The signin handler
///   in `server/src/serve/auth_routes.rs` writes the file when the request
///   came in over loopback, so a successful sign-in flips us to authed
///   without a second round trip.
const _: () = {
    use __cp::contract::{OrcaTool, OrcaToolDef};
    use __cp::dispatch::cli::{CliBuildFn, CliOp, CliRunFn};
    use ::plugin_toolkit as __cp;

    fn build() -> __cp::clap::Command {
        let cmd = __cp::clap::Command::new("login").about(<AuthLogin as OrcaToolDef>::DESCRIPTION);
        <<AuthLogin as OrcaToolDef>::Args as __cp::clap::Args>::augment_args(cmd)
    }

    fn run(
        m: &__cp::clap::ArgMatches,
        ctx: ::std::sync::Arc<__cp::contract::ToolCtx>,
    ) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = __cp::anyhow::Result<()>> + Send>>
    {
        let m = m.clone();
        Box::pin(async move {
            let peer = ctx.peer().map(|s| s.to_string());
            let args =
                <<AuthLogin as OrcaToolDef>::Args as __cp::clap::FromArgMatches>::from_arg_matches(
                    &m,
                )
                .map_err(|e| __cp::anyhow::anyhow!("{e}"))?;

            if peer.is_none() && args.username.is_none() && args.password.is_none() {
                browser_login_flow().await?;
                return Ok(());
            }

            let out: LoginOutput = if let Some(peer) = peer {
                __cp::dispatch::cli::exec_remote::<AuthLogin>(&peer, args, &ctx).await?
            } else if <AuthLogin as OrcaToolDef>::LOCAL_ONLY
                || !__cp::dispatch::cli::local_daemon_reachable()
            {
                <AuthLogin as OrcaTool>::run(args, &ctx).await?
            } else {
                __cp::dispatch::cli::exec_local_daemon::<AuthLogin>(args, &ctx).await?
            };
            let s = __cp::serde_json::to_string_pretty(&out)
                .unwrap_or_else(|e| format!("<unserializable output: {e}>"));
            println!("{s}");
            Ok(())
        })
    }

    __cp::inventory::submit! {
        CliOp {
            domain: "auth",
            verb: "login",
            summary: <AuthLogin as OrcaToolDef>::DESCRIPTION,
            build: build as CliBuildFn,
            run: run as CliRunFn,
        }
    }
};

async fn browser_login_flow() -> anyhow::Result<()> {
    use plugin_toolkit::dispatch::cli::local_daemon_url;

    if !plugin_toolkit::dispatch::cli::local_daemon_reachable() {
        anyhow::bail!(
            "local daemon not reachable at {} — start it with `orca daemon start`",
            local_daemon_url()
        );
    }

    let session_path = files::ops::orca_home()
        .map(|d| d.join("session"))
        .ok_or_else(|| anyhow::anyhow!("no ORCA_HOME/HOME — cannot persist session"))?;
    let prior = std::fs::read_to_string(&session_path)
        .ok()
        .map(|s| s.trim().to_string());

    let url = format!("{}/signin", local_daemon_url());
    println!("Opening {url} in your browser …");
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(&url).spawn() {
        println!("  (couldn't launch `{opener}`: {e}) — open the URL manually.");
    }

    println!("Waiting for sign-in …");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for sign-in after 5 minutes");
        }
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        let Ok(cur) = std::fs::read_to_string(&session_path) else {
            continue;
        };
        let cur = cur.trim().to_string();
        if cur.is_empty() {
            continue;
        }
        if prior.as_deref() == Some(cur.as_str()) {
            continue;
        }
        println!("Signed in — session mirrored to $ORCA_HOME/session.");
        return Ok(());
    }
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct LogoutArgs {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct LogoutOutput {
    pub revoked: bool,
}

/// [MUTATES STATE] Revoke the on-disk CLI session and remove
/// `$ORCA_HOME/session`. Idempotent — `revoked=false` means there was no
/// active session to clear.
#[orca_tool(domain = "auth", verb = "logout")]
async fn auth_logout(_args: LogoutArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<LogoutOutput> {
    let session_path = files::ops::orca_home().map(|d| d.join("session"));
    let mut revoked = false;
    if let Some(ref path) = session_path
        && let Ok(sid) = std::fs::read_to_string(path)
    {
        let sid = sid.trim();
        if !sid.is_empty() {
            let conn = db::open_default()?;
            let now = utils::time::now_rfc3339();
            revoked = crate::sessions::revoke(&conn, sid, &now)?;
        }
    }
    if let Some(path) = session_path
        && path.exists()
    {
        std::fs::remove_file(path)?;
    }
    Ok(LogoutOutput { revoked })
}

#[cfg(test)]
mod tests {
    use super::*;
    use utils::time::Timestamp;

    // ── validate_token_role ─────────────────────────────────────────────────

    #[test]
    fn validate_token_role_accepts_admin_and_read() {
        assert!(validate_token_role("admin").is_ok());
        assert!(validate_token_role("read").is_ok());
    }

    #[test]
    fn validate_token_role_rejects_other_and_reports_value() {
        for bad in ["", "Admin", "write", "readonly", "root", " read"] {
            let err = validate_token_role(bad).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("role must be 'admin' or 'read'"), "{msg}");
            assert!(msg.contains(bad), "error should echo the bad value: {msg}");
        }
    }

    // ── mint_token ──────────────────────────────────────────────────────────

    #[test]
    fn mint_token_has_orca_prefix_and_32_hex_body() {
        let raw = [0u8; 16];
        let (plaintext, _hash) = mint_token(&raw);
        assert!(plaintext.starts_with("orca_"));
        let body = plaintext.strip_prefix("orca_").unwrap();
        assert_eq!(body.len(), 32); // 16 bytes → 32 hex chars
        assert!(body.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(body, "00000000000000000000000000000000");
    }

    #[test]
    fn mint_token_hash_is_deterministic_and_sha256_of_plaintext() {
        let raw: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let (plaintext, token_hash) = mint_token(&raw);
        assert_eq!(plaintext, "orca_0123456789abcdeffedcba9876543210");
        // Hash is sha256 over the *plaintext* bytes, not the raw bytes.
        assert_eq!(token_hash, hash::sha256_hex(plaintext.as_bytes()));
        assert_eq!(token_hash.len(), 64);
        // Determinism: same input → same output.
        let (p2, h2) = mint_token(&raw);
        assert_eq!((plaintext, token_hash), (p2, h2));
    }

    #[test]
    fn mint_token_differs_with_input() {
        let (_, h_a) = mint_token(&[0u8; 16]);
        let (_, h_b) = mint_token(&[1u8; 16]);
        assert_ne!(h_a, h_b);
    }

    // ── compute_expires_at ──────────────────────────────────────────────────

    fn base() -> Timestamp {
        // 2021-01-01T00:00:00Z
        Timestamp::from_unix_seconds(1_609_459_200).unwrap()
    }

    #[test]
    fn compute_expires_at_none_never_expires() {
        assert_eq!(compute_expires_at(base(), None), None);
    }

    #[test]
    fn compute_expires_at_one_day_adds_86400s() {
        let got = compute_expires_at(base(), Some(1)).unwrap();
        assert_eq!(got, "2021-01-02T00:00:00Z");
    }

    #[test]
    fn compute_expires_at_thirty_days() {
        let got = compute_expires_at(base(), Some(30)).unwrap();
        assert_eq!(got, "2021-01-31T00:00:00Z");
    }

    #[test]
    fn compute_expires_at_zero_days_is_base_instant() {
        // A 0-day expiry maps to the base instant itself (edge, but well-defined).
        let got = compute_expires_at(base(), Some(0)).unwrap();
        assert_eq!(got, "2021-01-01T00:00:00Z");
    }

    #[test]
    fn compute_expires_at_is_parseable_rfc3339() {
        let got = compute_expires_at(base(), Some(7)).unwrap();
        let parsed = Timestamp::parse_rfc3339(&got).unwrap();
        assert_eq!(parsed.unix_seconds(), 1_609_459_200 + 7 * 86_400);
    }

    // ── serde round-trips of the wire rows ──────────────────────────────────

    #[test]
    fn api_token_summary_omits_none_options_and_defaults_can_mutate() {
        let row = ApiTokenSummary {
            id: "t1".into(),
            name: "ci".into(),
            role: "read".into(),
            created_at: "2021-01-01T00:00:00Z".into(),
            last_used_at: None,
            expires_at: None,
            can_mutate: false,
        };
        let v = serde_json::to_value(&row).unwrap();
        assert!(v.get("last_used_at").is_none());
        assert!(v.get("expires_at").is_none());
        assert_eq!(v["can_mutate"], serde_json::json!(false));
        // can_mutate defaults to false when absent on the wire.
        let back: ApiTokenSummary = serde_json::from_value(serde_json::json!({
            "id": "t1", "name": "ci", "role": "read",
            "created_at": "2021-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(!back.can_mutate);
        assert!(back.last_used_at.is_none());
    }

    #[test]
    fn token_create_args_defaults() {
        let a: TokenCreateArgs = serde_json::from_value(serde_json::json!({
            "name": "ci", "role": "admin"
        }))
        .unwrap();
        assert_eq!(a.expires_in_days, None);
        assert!(!a.can_mutate);
    }

    #[test]
    fn auth_login_output_serializes_all_fields() {
        let out = LoginOutput {
            user_id: "u1".into(),
            username: "scott".into(),
            role: "admin".into(),
            expires_at: "2021-01-02T00:00:00Z".into(),
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["user_id"], "u1");
        assert_eq!(v["username"], "scott");
        assert_eq!(v["role"], "admin");
        assert_eq!(v["expires_at"], "2021-01-02T00:00:00Z");
    }

    #[test]
    fn auth_provider_status_masks_identity_optionally() {
        let configured = AuthProviderStatus {
            provider: "anthropic".into(),
            configured: true,
            identity: Some("sk-…abcd".into()),
        };
        let v = serde_json::to_value(&configured).unwrap();
        assert_eq!(v["configured"], serde_json::json!(true));
        assert_eq!(v["identity"], "sk-…abcd");

        let unconfigured = AuthProviderStatus {
            provider: "github".into(),
            configured: false,
            identity: None,
        };
        let v = serde_json::to_value(&unconfigured).unwrap();
        assert!(v.get("identity").is_none());
    }

    #[test]
    fn cli_session_ttl_is_24h() {
        assert_eq!(CLI_SESSION_TTL_SECS, 86_400);
    }

    // ── serde: string-level round-trips (no serde_json::Value) ──────────────

    #[test]
    fn auth_provider_status_skips_none_identity_in_json() {
        let s = serde_json::to_string(&AuthProviderStatus {
            provider: "github".into(),
            configured: false,
            identity: None,
        })
        .unwrap();
        assert!(
            !s.contains("identity"),
            "None identity must be omitted: {s}"
        );
        assert!(s.contains("\"configured\":false"), "{s}");
        assert!(s.contains("\"provider\":\"github\""), "{s}");
    }

    #[test]
    fn auth_provider_status_emits_some_identity() {
        let s = serde_json::to_string(&AuthProviderStatus {
            provider: "anthropic".into(),
            configured: true,
            identity: Some("sk-…9f2a".into()),
        })
        .unwrap();
        assert!(s.contains("\"identity\":\"sk-…9f2a\""), "{s}");
        assert!(s.contains("\"configured\":true"), "{s}");
    }

    #[test]
    fn auth_status_report_nests_providers() {
        let s = serde_json::to_string(&AuthStatusReport {
            providers: vec![AuthProviderStatus {
                provider: "atlassian".into(),
                configured: false,
                identity: None,
            }],
        })
        .unwrap();
        assert!(s.starts_with("{\"providers\":["), "{s}");
        assert!(s.contains("\"provider\":\"atlassian\""), "{s}");
    }

    #[test]
    fn auth_logout_output_round_trips() {
        let s = serde_json::to_string(&AuthLogoutOutput {
            provider: "github".into(),
            removed: true,
        })
        .unwrap();
        assert_eq!(s, "{\"provider\":\"github\",\"removed\":true}");
        let back: AuthLogoutOutput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.provider, "github");
        assert!(back.removed);
    }

    #[test]
    fn auth_login_args_key_optional_and_skipped_when_none() {
        // `key` absent on the wire deserializes to None.
        let a: AuthLoginArgs = serde_json::from_str("{\"provider\":\"github\"}").unwrap();
        assert_eq!(a.provider, "github");
        assert_eq!(a.key, None);
        // and a None key is omitted on re-serialization.
        let s = serde_json::to_string(&a).unwrap();
        assert!(!s.contains("key"), "None key must be omitted: {s}");
    }

    #[test]
    fn auth_login_args_key_present_round_trips() {
        let a: AuthLoginArgs =
            serde_json::from_str("{\"provider\":\"anthropic\",\"key\":\"sk-abc\"}").unwrap();
        assert_eq!(a.key.as_deref(), Some("sk-abc"));
        let s = serde_json::to_string(&a).unwrap();
        assert!(s.contains("\"key\":\"sk-abc\""), "{s}");
    }

    #[test]
    fn auth_login_output_skips_none_identity() {
        let s = serde_json::to_string(&AuthLoginOutput {
            provider: "github".into(),
            stored: false,
            identity: None,
        })
        .unwrap();
        assert_eq!(s, "{\"provider\":\"github\",\"stored\":false}");
    }

    #[test]
    fn auth_login_output_emits_some_identity() {
        let s = serde_json::to_string(&AuthLoginOutput {
            provider: "anthropic".into(),
            stored: true,
            identity: Some("sk-…9f2a".into()),
        })
        .unwrap();
        assert!(s.contains("\"stored\":true"), "{s}");
        assert!(s.contains("\"identity\":\"sk-…9f2a\""), "{s}");
    }

    #[test]
    fn token_create_output_serializes_plaintext_once() {
        let s = serde_json::to_string(&TokenCreateOutput {
            id: "id1".into(),
            name: "ci".into(),
            token: "orca_deadbeef".into(),
        })
        .unwrap();
        assert_eq!(
            s,
            "{\"id\":\"id1\",\"name\":\"ci\",\"token\":\"orca_deadbeef\"}"
        );
    }

    #[test]
    fn token_create_args_full_round_trip() {
        let a: TokenCreateArgs = serde_json::from_str(
            "{\"name\":\"ci\",\"role\":\"read\",\"expires_in_days\":30,\"can_mutate\":true}",
        )
        .unwrap();
        assert_eq!(a.name, "ci");
        assert_eq!(a.role, "read");
        assert_eq!(a.expires_in_days, Some(30));
        assert!(a.can_mutate);
    }

    #[test]
    fn token_list_args_camel_case_and_defaults() {
        // camelCase rename + `default` container attr: empty object is valid.
        let empty: TokenListArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.limit, None);
        assert_eq!(empty.cursor, None);
        let a: TokenListArgs = serde_json::from_str("{\"limit\":25,\"cursor\":\"abc\"}").unwrap();
        assert_eq!(a.limit, Some(25));
        assert_eq!(a.cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn token_list_output_omits_none_cursor_and_total() {
        let s = serde_json::to_string(&TokenListOutput {
            tokens: vec![],
            next_cursor: None,
            total: None,
        })
        .unwrap();
        assert_eq!(s, "{\"tokens\":[]}");
        let s2 = serde_json::to_string(&TokenListOutput {
            tokens: vec![],
            next_cursor: Some("nc".into()),
            total: Some(3),
        })
        .unwrap();
        assert!(s2.contains("\"next_cursor\":\"nc\""), "{s2}");
        assert!(s2.contains("\"total\":3"), "{s2}");
    }

    #[test]
    fn api_token_summary_full_round_trip() {
        let src = "{\"id\":\"t1\",\"name\":\"ci\",\"role\":\"admin\",\
             \"created_at\":\"2021-01-01T00:00:00Z\",\
             \"last_used_at\":\"2021-02-01T00:00:00Z\",\
             \"expires_at\":\"2022-01-01T00:00:00Z\",\"can_mutate\":true}";
        let row: ApiTokenSummary = serde_json::from_str(src).unwrap();
        assert_eq!(row.last_used_at.as_deref(), Some("2021-02-01T00:00:00Z"));
        assert_eq!(row.expires_at.as_deref(), Some("2022-01-01T00:00:00Z"));
        assert!(row.can_mutate);
        let s = serde_json::to_string(&row).unwrap();
        assert!(
            s.contains("\"last_used_at\":\"2021-02-01T00:00:00Z\""),
            "{s}"
        );
        assert!(s.contains("\"expires_at\":\"2022-01-01T00:00:00Z\""), "{s}");
    }

    #[test]
    fn token_revoke_round_trips() {
        let a: TokenRevokeArgs = serde_json::from_str("{\"id\":\"tok-9\"}").unwrap();
        assert_eq!(a.id, "tok-9");
        let s = serde_json::to_string(&TokenRevokeOutput { revoked: false }).unwrap();
        assert_eq!(s, "{\"revoked\":false}");
    }

    #[test]
    fn login_args_optional_fields_skipped_when_none() {
        let a: LoginArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(a.username, None);
        assert_eq!(a.password, None);
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(s, "{}");
    }

    #[test]
    fn login_args_present_round_trips() {
        let a: LoginArgs =
            serde_json::from_str("{\"username\":\"scott\",\"password\":\"pw\"}").unwrap();
        assert_eq!(a.username.as_deref(), Some("scott"));
        assert_eq!(a.password.as_deref(), Some("pw"));
    }

    #[test]
    fn logout_output_round_trips() {
        let s = serde_json::to_string(&LogoutOutput { revoked: true }).unwrap();
        assert_eq!(s, "{\"revoked\":true}");
        let back: LogoutOutput = serde_json::from_str("{\"revoked\":false}").unwrap();
        assert!(!back.revoked);
    }

    // ── async error branches that bail before any DB / network access ────────

    fn test_ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        use std::path::PathBuf;
        use std::sync::Arc;
        contract::ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_create_unknown_provider_bails() {
        use contract::OrcaTool;
        let err = AuthSessionCreate::run(
            AuthLoginArgs {
                provider: "gitlab".into(),
                key: None,
            },
            &test_ctx(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("unknown provider 'gitlab'"), "{err}");
        assert!(err.contains("anthropic|github|atlassian"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_create_anthropic_requires_key() {
        use contract::OrcaTool;
        let err = AuthSessionCreate::run(
            AuthLoginArgs {
                provider: "anthropic".into(),
                key: None,
            },
            &test_ctx(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(
            err.contains("`key` is required when provider=anthropic"),
            "{err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_delete_unknown_provider_bails() {
        use contract::OrcaTool;
        let err = AuthSessionDelete::run(
            AuthLogoutArgs {
                provider: "gitlab".into(),
            },
            &test_ctx(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("unknown provider 'gitlab'"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn login_requires_username() {
        use contract::OrcaTool;
        let err = AuthLogin::run(
            LoginArgs {
                username: None,
                password: Some("pw".into()),
            },
            &test_ctx(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("username required"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn login_requires_password_when_username_present() {
        use contract::OrcaTool;
        let err = AuthLogin::run(
            LoginArgs {
                username: Some("scott".into()),
                password: None,
            },
            &test_ctx(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("password required"), "{err}");
    }
}
