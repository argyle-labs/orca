//! Shared test infrastructure — in-memory / tempfile test databases and row
//! factories. Compiled for this crate's own tests and, behind the
//! `test-support` feature, for downstream crates' `[dev-dependencies]`.
//!
//! Composition over inheritance: `TestDb` owns the storage lifetime, `Seed`
//! borrows any connection and stamps rows through the SAME public CRUD
//! functions production uses — factories never write SQL of their own, so a
//! schema change that breaks production code breaks the factory the same way.

use crate::{apply_schema, apply_tuning_pragmas, run_pending_migrations};
use rusqlite::Connection;

/// Open an unencrypted in-memory database with full schema + migrations
/// applied. The default for unit tests: fastest, fully isolated per call.
pub fn mem_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("open_in_memory");
    // In-memory dbs ignore journal_mode=WAL and mmap_size, but the rest
    // (synchronous, cache_size, temp_store, busy_timeout) all apply.
    // Calling the same helper keeps test + prod configuration aligned.
    apply_tuning_pragmas(&conn).expect("apply_tuning_pragmas");
    apply_schema(&conn).expect("apply_schema");
    run_pending_migrations(&conn).expect("migrations");
    conn
}

/// A tempfile-backed test database for code that reaches the db through
/// `open_default()` rather than taking a `&Connection`.
///
/// Compose with the path-override seams:
/// - async tool bodies: `db::with_db_path(tdb.path(), async { ... }).await`
/// - sync fns:          `db::with_thread_db_path(&tdb.path(), || ...)`
///
/// The tempdir lives as long as the `TestDb` value — keep it in scope.
pub struct TestDb {
    dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl TestDb {
    pub fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("test.db");
        // Open once so schema + migrations exist before the code under test
        // connects, mirroring a daemon that already ran first boot.
        let _ = crate::open_unencrypted(&path).expect("open_unencrypted");
        TestDb { dir, path }
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.path.clone()
    }

    /// A direct connection for seeding/asserting around the code under test.
    pub fn conn(&self) -> Connection {
        crate::open_unencrypted(&self.path).expect("open_unencrypted")
    }

    pub fn dir(&self) -> &std::path::Path {
        self.dir.path()
    }
}

impl Default for TestDb {
    fn default() -> Self {
        Self::new()
    }
}

/// Row factories. Borrow any connection; every method routes through the
/// public CRUD layer with sensible defaults so a test states only what it
/// cares about.
pub struct Seed<'c>(pub &'c Connection);

impl<'c> Seed<'c> {
    pub fn user(&self, id: &str) -> crate::users::User {
        crate::users::insert(self.0, id, id, "x-hash", "admin", "2026-01-01T00:00:00Z")
            .expect("seed user")
    }

    pub fn profile(&self, id: &str, owner: &str) -> crate::profiles::ProfileRow {
        crate::profiles::create(self.0, id, id, owner, None).expect("seed profile")
    }

    /// Register a model row. `provider` ∈ anthropic | lmstudio | ollama | claude-code.
    pub fn model(&self, conn: &mut Connection, id: &str, provider: &str, is_default: bool) {
        let m = crate::models::Model {
            id: id.to_string(),
            provider: provider.to_string(),
            endpoint: match provider {
                "lmstudio" => Some("http://localhost:1234".into()),
                "ollama" => Some("http://localhost:11434".into()),
                _ => None,
            },
            model_name: format!("{id}-model"),
            is_default,
            enabled: true,
            created_at: String::new(), // stamped by insert
        };
        crate::models::insert(conn, &m).expect("seed model");
    }

    pub fn api_token(&self, id: &str, hash: &str) -> crate::api_tokens::ApiToken {
        crate::api_tokens::insert(
            self.0,
            id,
            id,
            hash,
            "admin",
            "2026-01-01T00:00:00Z",
            None,
            None,
        )
        .expect("seed api token")
    }

    pub fn session(&self, id: &str, user_id: &str) -> crate::sessions::Session {
        crate::sessions::insert(
            self.0,
            id,
            user_id,
            "2026-01-01T00:00:00Z",
            "2027-01-01T00:00:00Z",
        )
        .expect("seed session")
    }
}

impl<'c> Seed<'c> {
    /// Register a minimal enabled plugin row.
    pub fn plugin(&self, id: &str) -> crate::plugins::PluginRow {
        let row = crate::plugins::PluginRow {
            id: id.to_string(),
            manifest_path: format!("/tmp/{id}/orca-plugin.toml"),
            tier: "service".to_string(),
            context_injection: String::new(),
            enabled: true,
            command_map: Default::default(),
            nav_links: Vec::new(),
            search_tools: Vec::new(),
            specs_dir: None,
        };
        crate::plugins::upsert(self.0, &row).expect("seed plugin");
        row
    }
}
