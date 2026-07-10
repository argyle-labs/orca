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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct TokenListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenListOutput {
    pub tokens: Vec<ApiTokenSummary>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct TokenRevokeArgs {
    pub id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenRevokeOutput {
    pub revoked: bool,
}

/// [MUTATES STATE] Mint a new REST/MCP bearer token on THIS host. Plaintext is
/// returned exactly once and cannot be recovered from the DB. Token only
/// authenticates calls to this host's `:12000` — not to other peers.
#[orca_tool(domain = "auth.token", verb = "create")]
async fn auth_token_create(
    args: TokenCreateArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenCreateOutput> {
    if !matches!(args.role.as_str(), "admin" | "read") {
        bail!("role must be 'admin' or 'read', got '{}'", args.role);
    }
    // 16 random bytes → 32 hex chars. `orca_` prefix keeps tokens
    // self-identifying in logs/secret-scanners.
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let plaintext = format!("orca_{}", hash::hex_encode(&raw));
    let token_hash = hash::sha256_hex(plaintext.as_bytes());

    let id = utils::id::new();
    let now = utils::time::now_rfc3339();
    let expires_at = args.expires_in_days.map(|d| {
        utils::time::now()
            .plus(std::time::Duration::from_secs(d as u64 * 86_400))
            .to_rfc3339()
    });

    // Bind the new token to the authenticated operator so later bearer-auth
    // requests resolve to a real user (S4 of [[project-remote-exec-full-fix]]).
    // The bootstrap path (first token, no auth yet) has no caller → user_id
    // is NULL and that token authenticates only locally.
    let caller_user_id = ctx.caller().map(|c| c.user_id);
    let conn = db::open_default()?;
    db::api_tokens::insert(
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
    _args: TokenListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenListOutput> {
    let conn = db::open_default()?;
    let rows = db::api_tokens::list(&conn)?;
    let tokens = rows
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
    Ok(TokenListOutput { tokens })
}

/// [MUTATES STATE] Revoke a token by id. Returns `revoked=false` if the id wasn't found.
#[orca_tool(domain = "auth.token", verb = "delete")]
async fn auth_token_delete(
    args: TokenRevokeArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenRevokeOutput> {
    let conn = db::open_default()?;
    let revoked = db::api_tokens::revoke(&conn, &args.id)?;
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
    if let crate::throttle::CheckOutcome::Throttled { retry_after_secs } =
        crate::throttle::check(ip, username)
    {
        bail!("signin throttled — retry in {retry_after_secs}s");
    }

    let conn = db::open_default()?;
    let row = match db::users::find_auth_by_username(&conn, username)? {
        Some(r) => r,
        None => {
            crate::throttle::record_failure(ip, username);
            bail!("invalid credentials");
        }
    };
    let ok = crate::password::verify_password(password, &row.password_hash).unwrap_or(false);
    if !ok {
        crate::throttle::record_failure(ip, username);
        bail!("invalid credentials");
    }
    crate::throttle::record_success(ip, username);

    let session_path = files::ops::orca_home()
        .map(|d| d.join("session"))
        .ok_or_else(|| anyhow::anyhow!("no ORCA_HOME/HOME — cannot persist session"))?;

    // Single-session model: revoke any previous CLI session for this host.
    if let Ok(prev) = std::fs::read_to_string(&session_path) {
        let prev = prev.trim();
        if !prev.is_empty() {
            let now = utils::time::now_rfc3339();
            db::sessions::revoke(&conn, prev, &now).ok();
        }
    }

    let mut sid_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut sid_bytes);
    let sid = hash::hex_encode(&sid_bytes);
    let now = utils::time::now();
    let exp = now.plus(std::time::Duration::from_secs(CLI_SESSION_TTL_SECS as u64));
    db::sessions::insert(&conn, &sid, &row.id, &now.to_rfc3339(), &exp.to_rfc3339())?;

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
            revoked = db::sessions::revoke(&conn, sid, &now)?;
        }
    }
    if let Some(path) = session_path
        && path.exists()
    {
        std::fs::remove_file(path)?;
    }
    Ok(LogoutOutput { revoked })
}
