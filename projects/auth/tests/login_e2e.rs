//! End-to-end test of `auth.login` / `auth.logout`: real argon2 verify,
//! real `sessions` row insert, real session file on disk. Pins the contract
//! [[project-orca-login-local-auth]] depends on.

use auth::auth::{AuthLogin, AuthLogout, LoginArgs, LoginOutput, LogoutArgs, LogoutOutput};
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
    let id = uuid::Uuid::now_v7().to_string();
    db::users::insert(&conn, &id, username, &hash, "admin", &now).unwrap();
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
    let row = db::sessions::find_active(&conn, sid.trim())
        .unwrap()
        .expect("session row");
    assert_eq!(row.user_id, uid);

    let out = logout().await.unwrap();
    assert!(out.revoked, "logout should revoke the active session");
    assert!(!path.exists(), "session file should be removed");
    assert!(
        db::sessions::find_active(&conn, sid.trim())
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
        db::sessions::find_active(&conn, &sid1).unwrap().is_none(),
        "prior session must be revoked"
    );
    assert!(
        db::sessions::find_active(&conn, &sid2).unwrap().is_some(),
        "new session must be active"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn logout_with_no_session_is_noop() {
    let _h = fixture_home();
    let out = logout().await.unwrap();
    assert!(!out.revoked);
}
