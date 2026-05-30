//! End-to-end test of `auth.login` / `auth.logout`: real argon2 verify,
//! real `sessions` row insert, real session file on disk. Pins the contract
//! [[project-orca-login-local-auth]] depends on.

use auth::auth::{AuthLogin, AuthLogout, LoginArgs, LogoutArgs};
use contract::OrcaTool;
use contract::ToolCtx;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use utils::config::{Config, Model};

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

fn fixture_home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("orca.db");
    // SAFETY: env mutation in tests is single-threaded by default in this
    // integration binary (`flavor = "current_thread"` + cargo's
    // per-binary test scheduling), and each test re-pins these to its own
    // fresh tempdir before doing any work.
    unsafe {
        std::env::set_var("ORCA_HOME", dir.path());
        std::env::set_var("HOME", dir.path());
        std::env::set_var("ORCA_DB_PATH", &db_path);
    }
    dir
}

fn seed_admin(username: &str, password: &str) -> String {
    let conn = db::open_default().unwrap();
    let hash = auth::password::hash_password(password).unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::now_v7().to_string();
    db::users::insert(&conn, &id, username, &hash, "admin", &now).unwrap();
    id
}

async fn invoke_login(username: &str, password: &str) -> anyhow::Result<Value> {
    let args = LoginArgs {
        username: username.into(),
        password: password.into(),
    };
    let ctx = make_ctx();
    let out = AuthLogin::run(args, &ctx).await?;
    Ok(serde_json::to_value(&out)?)
}

async fn invoke_logout() -> anyhow::Result<Value> {
    let ctx = make_ctx();
    let out = AuthLogout::run(LogoutArgs {}, &ctx).await?;
    Ok(serde_json::to_value(&out)?)
}

#[tokio::test(flavor = "current_thread")]
async fn login_then_logout_roundtrips() {
    let _h = fixture_home();
    let uid = seed_admin("alice", "hunter2");

    // login
    let v = invoke_login("alice", "hunter2").await.unwrap();
    assert_eq!(v["user_id"].as_str(), Some(uid.as_str()));
    assert_eq!(v["username"].as_str(), Some("alice"));
    assert_eq!(v["role"].as_str(), Some("admin"));

    // session file exists, mode 0600 (unix)
    let path = utils::fs::orca_home().unwrap().join("session");
    assert!(path.exists(), "session file should exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "session file must be 0600");
    }

    // session row is active in DB
    let sid = std::fs::read_to_string(&path).unwrap();
    let conn = db::open_default().unwrap();
    let row = db::sessions::find_active(&conn, sid.trim())
        .unwrap()
        .expect("session row");
    assert_eq!(row.user_id, uid);

    // logout revokes + clears file
    let v = invoke_logout().await.unwrap();
    assert_eq!(v["revoked"].as_bool(), Some(true));
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
    let err = invoke_login("bob", "wrong").await.unwrap_err();
    assert!(
        err.to_string().contains("invalid credentials"),
        "got: {err}"
    );
    let path = utils::fs::orca_home().unwrap().join("session");
    assert!(!path.exists(), "no session file on failed login");
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_user_rejected() {
    let _h = fixture_home();
    let err = invoke_login("ghost", "anything").await.unwrap_err();
    assert!(
        err.to_string().contains("invalid credentials"),
        "got: {err}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn second_login_revokes_prior_session() {
    let _h = fixture_home();
    seed_admin("carol", "pw1");
    let v1 = invoke_login("carol", "pw1").await.unwrap();
    let path = utils::fs::orca_home().unwrap().join("session");
    let sid1 = std::fs::read_to_string(&path).unwrap().trim().to_string();
    assert_eq!(v1["username"].as_str(), Some("carol"));

    let _ = invoke_login("carol", "pw1").await.unwrap();
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
    let v = invoke_logout().await.unwrap();
    assert_eq!(v["revoked"].as_bool(), Some(false));
}
