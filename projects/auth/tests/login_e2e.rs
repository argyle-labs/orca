//! End-to-end test of `auth.login` / `auth.logout`: real argon2 verify,
//! real `sessions` row insert, real session file on disk. Pins the contract
//! [[project-orca-login-local-auth]] depends on.

use auth::auth::{
    AuthLogin, AuthLoginArgs, AuthLogout, AuthLogoutArgs, AuthSessionCreate, AuthSessionDelete,
    AuthSessionDetail, AuthStatusArgs, AuthTokenCreate, AuthTokenDelete, AuthTokenList, LoginArgs,
    LoginOutput, LogoutArgs, LogoutOutput, TokenCreateArgs, TokenListArgs, TokenRevokeArgs,
};
use contract::OrcaTool;
use contract::ToolCtx;
use contract::config::{Config, Model};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Global serialization for tests that mutate process-wide env vars
/// (ORCA_HOME, HOME, ORCA_DB_PATH). Without this, parallel tests race
/// and stomp each other's tempdirs → "database is locked" / "disk I/O error".
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub struct Fixture {
    _guard: MutexGuard<'static, ()>,
    pub dir: tempfile::TempDir,
}

fn make_ctx() -> ToolCtx {
    ToolCtx::new(Arc::new(Config {
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

fn fixture_home() -> Fixture {
    // Hold the env lock for the entire test so parallel tests don't stomp
    // ORCA_HOME / HOME / ORCA_DB_PATH on each other (each pins them to its
    // own tempdir; without serialization sqlite races on the same file).
    let guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("orca.db");
    // SAFETY: env mutation is serialized by `env_lock()` above.
    unsafe {
        std::env::set_var("ORCA_HOME", dir.path());
        std::env::set_var("HOME", dir.path());
        std::env::set_var("ORCA_DB_PATH", &db_path);
    }
    Fixture { _guard: guard, dir }
}

fn seed_admin(username: &str, password: &str) -> String {
    let conn = db::open_default().unwrap();
    let hash = auth::password::hash_password(password).unwrap();
    let now = utils::time::now_rfc3339();
    let id = utils::id::new();
    auth::users::insert(&conn, &id, username, &hash, "admin", &now).unwrap();
    id
}

async fn login(username: &str, password: &str) -> anyhow::Result<LoginOutput> {
    AuthLogin::run(
        LoginArgs {
            username: Some(username.into()),
            password: Some(password.into()),
        },
        &make_ctx(),
    )
    .await
}

async fn logout() -> anyhow::Result<LogoutOutput> {
    AuthLogout::run(LogoutArgs {}, &make_ctx()).await
}

#[tokio::test(flavor = "current_thread")]
async fn login_then_logout_roundtrips() {
    let _h = fixture_home();
    let uid = seed_admin("alice", "hunter2");

    let out = login("alice", "hunter2").await.unwrap();
    assert_eq!(out.user_id, uid);
    assert_eq!(out.username, "alice");
    assert_eq!(out.role, "admin");

    let path = files::ops::orca_home().unwrap().join("session");
    assert!(path.exists(), "session file should exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "session file must be 0600");
    }

    let sid = std::fs::read_to_string(&path).unwrap();
    let conn = db::open_default().unwrap();
    let row = auth::sessions::find_active(&conn, sid.trim())
        .unwrap()
        .expect("session row");
    assert_eq!(row.user_id, uid);

    let out = logout().await.unwrap();
    assert!(out.revoked, "logout should revoke the active session");
    assert!(!path.exists(), "session file should be removed");
    assert!(
        auth::sessions::find_active(&conn, sid.trim())
            .unwrap()
            .is_none(),
        "session row should be revoked"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_password_rejected() {
    let _h = fixture_home();
    seed_admin("bob", "correct-horse");
    let err = login("bob", "wrong").await.unwrap_err();
    assert!(
        err.to_string().contains("invalid credentials"),
        "got: {err}"
    );
    let path = files::ops::orca_home().unwrap().join("session");
    assert!(!path.exists(), "no session file on failed login");
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_user_rejected() {
    let _h = fixture_home();
    let err = login("ghost", "anything").await.unwrap_err();
    assert!(
        err.to_string().contains("invalid credentials"),
        "got: {err}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn second_login_revokes_prior_session() {
    let _h = fixture_home();
    seed_admin("carol", "pw1");
    let first = login("carol", "pw1").await.unwrap();
    assert_eq!(first.username, "carol");
    let path = files::ops::orca_home().unwrap().join("session");
    let sid1 = std::fs::read_to_string(&path).unwrap().trim().to_string();

    let _ = login("carol", "pw1").await.unwrap();
    let sid2 = std::fs::read_to_string(&path).unwrap().trim().to_string();
    assert_ne!(sid1, sid2, "second login mints a fresh sid");

    let conn = db::open_default().unwrap();
    assert!(
        auth::sessions::find_active(&conn, &sid1).unwrap().is_none(),
        "prior session must be revoked"
    );
    assert!(
        auth::sessions::find_active(&conn, &sid2).unwrap().is_some(),
        "new session must be active"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn logout_with_no_session_is_noop() {
    let _h = fixture_home();
    let out = logout().await.unwrap();
    assert!(!out.revoked);
}

// ── auth.session (provider credentials) ─────────────────────────────────────

/// Full anthropic credential lifecycle across the three `auth.session` verbs:
/// create stores + masks, detail reflects `configured=true`, delete removes it,
/// and a second detail flips back to `configured=false`.
#[tokio::test(flavor = "current_thread")]
async fn anthropic_session_create_detail_delete_lifecycle() {
    let _h = fixture_home();
    let ctx = make_ctx();

    // Nothing configured yet.
    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let anthropic = report
        .providers
        .iter()
        .find(|p| p.provider == "anthropic")
        .expect("anthropic row present");
    assert!(!anthropic.configured);
    assert!(anthropic.identity.is_none());
    // Every provider row is always reported, even when unconfigured.
    for want in ["anthropic", "github", "atlassian"] {
        assert!(
            report.providers.iter().any(|p| p.provider == want),
            "missing provider row {want}"
        );
    }

    // Store a key.
    let out = AuthSessionCreate::run(
        AuthLoginArgs {
            provider: "anthropic".into(),
            key: Some("sk-ant-secret-1234567890".into()),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(out.provider, "anthropic");
    assert!(out.stored);
    let identity = out.identity.expect("masked identity returned");
    // Masking must not leak the full key back.
    assert_ne!(identity, "sk-ant-secret-1234567890");
    assert!(!identity.is_empty());

    // Detail now reports it configured with the same masked identity.
    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let anthropic = report
        .providers
        .iter()
        .find(|p| p.provider == "anthropic")
        .unwrap();
    assert!(anthropic.configured);
    assert_eq!(anthropic.identity.as_deref(), Some(identity.as_str()));

    // Delete removes it.
    let del = AuthSessionDelete::run(
        AuthLogoutArgs {
            provider: "anthropic".into(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(del.provider, "anthropic");
    assert!(del.removed);

    // Deleting again is idempotent: nothing left to remove.
    let del2 = AuthSessionDelete::run(
        AuthLogoutArgs {
            provider: "anthropic".into(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(!del2.removed);

    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let anthropic = report
        .providers
        .iter()
        .find(|p| p.provider == "anthropic")
        .unwrap();
    assert!(!anthropic.configured);
}

/// A seeded github OAuth row surfaces in `auth.session detail` and can be
/// dropped via `auth.session delete` (exercises the OAuth-provider branches
/// without a live device flow).
#[tokio::test(flavor = "current_thread")]
async fn github_oauth_session_detail_and_delete() {
    let _h = fixture_home();
    let conn = db::open_default().unwrap();
    auth::oauth_store::upsert(
        &conn,
        &auth::oauth_store::TokenRow {
            service: "github".into(),
            access_token: "gho_test_access_token_abcdef".into(),
            refresh_token: None,
            expires_at: None,
        },
    )
    .unwrap();

    let ctx = make_ctx();
    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let github = report
        .providers
        .iter()
        .find(|p| p.provider == "github")
        .unwrap();
    assert!(github.configured);
    assert!(github.identity.is_some());

    let del = AuthSessionDelete::run(
        AuthLogoutArgs {
            provider: "github".into(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(del.removed);

    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let github = report
        .providers
        .iter()
        .find(|p| p.provider == "github")
        .unwrap();
    assert!(!github.configured);
}

#[tokio::test(flavor = "current_thread")]
async fn session_create_anthropic_without_key_errors() {
    let _h = fixture_home();
    let err = AuthSessionCreate::run(
        AuthLoginArgs {
            provider: "anthropic".into(),
            key: None,
        },
        &make_ctx(),
    )
    .await
    .err()
    .expect("expected error");
    assert!(err.to_string().contains("`key` is required"), "got: {err}");
}

#[tokio::test(flavor = "current_thread")]
async fn session_create_unknown_provider_errors() {
    let _h = fixture_home();
    let err = AuthSessionCreate::run(
        AuthLoginArgs {
            provider: "gitlab".into(),
            key: None,
        },
        &make_ctx(),
    )
    .await
    .err()
    .expect("expected error");
    let msg = err.to_string();
    assert!(msg.contains("unknown provider 'gitlab'"), "got: {msg}");
    assert!(msg.contains("anthropic|github|atlassian"), "got: {msg}");
}

#[tokio::test(flavor = "current_thread")]
async fn session_delete_unknown_provider_errors() {
    let _h = fixture_home();
    let err = AuthSessionDelete::run(
        AuthLogoutArgs {
            provider: "gitlab".into(),
        },
        &make_ctx(),
    )
    .await
    .err()
    .expect("expected error");
    assert!(
        err.to_string().contains("unknown provider 'gitlab'"),
        "got: {err}"
    );
}

// ── auth.token (bearer tokens) ──────────────────────────────────────────────

/// Mint → list → revoke lifecycle for a bearer token, asserting the plaintext
/// shape, the round-tripped summary fields, and idempotent revoke.
#[tokio::test(flavor = "current_thread")]
async fn token_create_list_revoke_lifecycle() {
    let _h = fixture_home();
    let ctx = make_ctx();

    let created = AuthTokenCreate::run(
        TokenCreateArgs {
            name: "ci-runner".into(),
            role: "read".into(),
            expires_in_days: Some(30),
            can_mutate: true,
        },
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(created.name, "ci-runner");
    assert!(created.token.starts_with("orca_"));
    assert!(!created.id.is_empty());

    let listed = AuthTokenList::run(TokenListArgs::default(), &ctx)
        .await
        .unwrap();
    let row = listed
        .tokens
        .iter()
        .find(|t| t.id == created.id)
        .expect("created token present in list");
    assert_eq!(row.name, "ci-runner");
    assert_eq!(row.role, "read");
    assert!(row.can_mutate);
    assert!(row.expires_at.is_some());
    // The list surface never leaks the plaintext or hash.
    let listed_json = serde_json::to_string(&listed).unwrap();
    assert!(!listed_json.contains(&created.token));

    let revoked = AuthTokenDelete::run(
        TokenRevokeArgs {
            id: created.id.clone(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(revoked.revoked);

    // Revoking a second time reports nothing was removed.
    let revoked2 = AuthTokenDelete::run(
        TokenRevokeArgs {
            id: created.id.clone(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(!revoked2.revoked);

    let listed = AuthTokenList::run(TokenListArgs::default(), &ctx)
        .await
        .unwrap();
    assert!(
        !listed.tokens.iter().any(|t| t.id == created.id),
        "revoked token must be gone from the list"
    );
}

/// A `None` expiry mints a never-expiring token (the `expires_at` column stays
/// absent on the summary).
#[tokio::test(flavor = "current_thread")]
async fn token_create_without_expiry_never_expires() {
    let _h = fixture_home();
    let ctx = make_ctx();
    let created = AuthTokenCreate::run(
        TokenCreateArgs {
            name: "forever".into(),
            role: "admin".into(),
            expires_in_days: None,
            can_mutate: false,
        },
        &ctx,
    )
    .await
    .unwrap();
    let listed = AuthTokenList::run(TokenListArgs::default(), &ctx)
        .await
        .unwrap();
    let row = listed.tokens.iter().find(|t| t.id == created.id).unwrap();
    assert!(row.expires_at.is_none());
    assert_eq!(row.role, "admin");
    assert!(!row.can_mutate);
}

#[tokio::test(flavor = "current_thread")]
async fn token_create_rejects_bad_role_before_touching_db() {
    let _h = fixture_home();
    let err = AuthTokenCreate::run(
        TokenCreateArgs {
            name: "bad".into(),
            role: "superuser".into(),
            expires_in_days: None,
            can_mutate: false,
        },
        &make_ctx(),
    )
    .await
    .err()
    .expect("expected error");
    assert!(
        err.to_string().contains("role must be 'admin' or 'read'"),
        "got: {err}"
    );
    // The rejected mint left no row behind.
    let listed = AuthTokenList::run(TokenListArgs::default(), &make_ctx())
        .await
        .unwrap();
    assert!(listed.tokens.iter().all(|t| t.name != "bad"));
}

/// Listing is stable-sorted by id and honors the page limit.
#[tokio::test(flavor = "current_thread")]
async fn token_list_sorted_by_id_and_paginated() {
    let _h = fixture_home();
    let ctx = make_ctx();
    for i in 0..3 {
        AuthTokenCreate::run(
            TokenCreateArgs {
                name: format!("tok-{i}"),
                role: "read".into(),
                expires_in_days: None,
                can_mutate: false,
            },
            &ctx,
        )
        .await
        .unwrap();
    }
    let page = AuthTokenList::run(
        TokenListArgs {
            limit: Some(2),
            cursor: None,
        },
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(page.tokens.len(), 2, "limit clamps the page size");
    assert_eq!(page.total, Some(3), "total counts across pages");
    assert!(page.next_cursor.is_some(), "more pages remain");
    // Ascending id order within the page.
    assert!(page.tokens[0].id <= page.tokens[1].id);
}

/// Following `next_cursor` returns the remaining rows exactly once: the two
/// pages together cover all ids with no overlap, and the final page reports no
/// further cursor.
#[tokio::test(flavor = "current_thread")]
async fn token_list_cursor_follows_to_final_page() {
    let _h = fixture_home();
    let ctx = make_ctx();
    for i in 0..5 {
        AuthTokenCreate::run(
            TokenCreateArgs {
                name: format!("page-tok-{i}"),
                role: "read".into(),
                expires_in_days: None,
                can_mutate: false,
            },
            &ctx,
        )
        .await
        .unwrap();
    }

    let first = AuthTokenList::run(
        TokenListArgs {
            limit: Some(3),
            cursor: None,
        },
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(first.tokens.len(), 3);
    let cursor = first.next_cursor.clone().expect("first page has a cursor");

    let second = AuthTokenList::run(
        TokenListArgs {
            limit: Some(3),
            cursor: Some(cursor),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(second.tokens.len(), 2, "remaining rows on the final page");
    assert!(
        second.next_cursor.is_none(),
        "final page must not advertise another cursor"
    );

    // Union covers all five ids with no duplicates across the page boundary.
    let mut ids: Vec<String> = first
        .tokens
        .iter()
        .chain(second.tokens.iter())
        .map(|t| t.id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "pages partition the rows with no overlap");
    // Global ordering is preserved across the boundary.
    assert!(first.tokens.last().unwrap().id <= second.tokens.first().unwrap().id);
}

/// An out-of-range page limit is clamped rather than rejected: a huge limit
/// still returns every row on a single page with no trailing cursor.
#[tokio::test(flavor = "current_thread")]
async fn token_list_large_limit_returns_all_on_one_page() {
    let _h = fixture_home();
    let ctx = make_ctx();
    for i in 0..4 {
        AuthTokenCreate::run(
            TokenCreateArgs {
                name: format!("all-tok-{i}"),
                role: "read".into(),
                expires_in_days: None,
                can_mutate: false,
            },
            &ctx,
        )
        .await
        .unwrap();
    }
    let page = AuthTokenList::run(
        TokenListArgs {
            limit: Some(10_000),
            cursor: None,
        },
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(page.tokens.len(), 4);
    assert_eq!(page.total, Some(4));
    assert!(page.next_cursor.is_none());
}

/// A second anthropic `session create` overwrites the stored key in place: the
/// masked identity tracks the new value and only one credential remains.
#[tokio::test(flavor = "current_thread")]
async fn anthropic_session_create_overwrites_existing_key() {
    let _h = fixture_home();
    let ctx = make_ctx();

    let first = AuthSessionCreate::run(
        AuthLoginArgs {
            provider: "anthropic".into(),
            key: Some("sk-ant-first-000000000000".into()),
        },
        &ctx,
    )
    .await
    .unwrap();
    let second = AuthSessionCreate::run(
        AuthLoginArgs {
            provider: "anthropic".into(),
            key: Some("sk-ant-second-999999999999".into()),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(second.stored);
    assert_ne!(
        first.identity, second.identity,
        "masked identity reflects the replacement key"
    );

    // Detail reports the latest masked identity, and a single delete clears it.
    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let anthropic = report
        .providers
        .iter()
        .find(|p| p.provider == "anthropic")
        .unwrap();
    assert!(anthropic.configured);
    assert_eq!(anthropic.identity, second.identity);

    let del = AuthSessionDelete::run(
        AuthLogoutArgs {
            provider: "anthropic".into(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(del.removed);
    let del_again = AuthSessionDelete::run(
        AuthLogoutArgs {
            provider: "anthropic".into(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(
        !del_again.removed,
        "only one credential existed after the overwrite"
    );
}

/// A seeded atlassian OAuth token surfaces in `detail` and is dropped by
/// `delete`, exercising the atlassian provider branch without a live PKCE flow.
#[tokio::test(flavor = "current_thread")]
async fn atlassian_oauth_session_detail_and_delete() {
    let _h = fixture_home();
    let conn = db::open_default().unwrap();
    auth::oauth_store::upsert(
        &conn,
        &auth::oauth_store::TokenRow {
            service: "atlassian".into(),
            access_token: "atl_test_access_token_1234567890".into(),
            refresh_token: Some("atl_refresh_abc".into()),
            expires_at: None,
        },
    )
    .unwrap();

    let ctx = make_ctx();
    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let atlassian = report
        .providers
        .iter()
        .find(|p| p.provider == "atlassian")
        .unwrap();
    assert!(atlassian.configured);
    assert!(atlassian.identity.is_some());

    let del = AuthSessionDelete::run(
        AuthLogoutArgs {
            provider: "atlassian".into(),
        },
        &ctx,
    )
    .await
    .unwrap();
    assert!(del.removed);

    let report = AuthSessionDetail::run(AuthStatusArgs {}, &ctx)
        .await
        .unwrap();
    let atlassian = report
        .providers
        .iter()
        .find(|p| p.provider == "atlassian")
        .unwrap();
    assert!(!atlassian.configured);
}

/// Two tokens sharing a role but differing in name/mutate flags both round-trip
/// through the list surface with their fields intact.
#[tokio::test(flavor = "current_thread")]
async fn token_list_reflects_per_row_can_mutate() {
    let _h = fixture_home();
    let ctx = make_ctx();
    let mutating = AuthTokenCreate::run(
        TokenCreateArgs {
            name: "mutator".into(),
            role: "read".into(),
            expires_in_days: None,
            can_mutate: true,
        },
        &ctx,
    )
    .await
    .unwrap();
    let readonly = AuthTokenCreate::run(
        TokenCreateArgs {
            name: "readonly".into(),
            role: "read".into(),
            expires_in_days: None,
            can_mutate: false,
        },
        &ctx,
    )
    .await
    .unwrap();

    let listed = AuthTokenList::run(TokenListArgs::default(), &ctx)
        .await
        .unwrap();
    let m = listed.tokens.iter().find(|t| t.id == mutating.id).unwrap();
    let r = listed.tokens.iter().find(|t| t.id == readonly.id).unwrap();
    assert!(m.can_mutate);
    assert!(!r.can_mutate);
    assert_ne!(mutating.token, readonly.token, "each mint is unique");
}
