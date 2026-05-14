//! Encrypted SQLite database (`orca.db`) — the runtime registry for all dynamic orca config.
//!
//! `open_default()` is the standard entry point. It opens (or creates) `~/.orca/orca.db`,
//! applies the SQLCipher encryption key, runs `apply_schema` to ensure all tables exist,
//! then applies any pending schema migrations via `run_pending_migrations`.
//!
//! Adding a new registry feature means adding a table in `apply_schema`, CRUD helpers at
//! the bottom of this file, and a migration entry in `MIGRATIONS` if the table was added
//! to an already-deployed database.

pub mod config_store;
pub mod docker_runtimes;
pub mod docs;
pub mod home_assistant;
pub mod host_addressing;
pub mod llm;
pub mod mcp_servers;
pub mod oauth;
pub mod openapi_specs;
pub mod plugin_creds;
pub mod plugin_data;
pub mod plugin_installs;
pub mod plugin_tools;
pub mod plugin_types;
pub mod plugins;
pub mod pod;
pub mod profile_creds;
pub mod profiles;
pub mod proxmox;
pub mod scheduler_runs;
pub mod schema_databases;
pub mod secrets;
pub mod settings;
pub mod startup;
pub mod tool_mappings;

use anyhow::{Context, Result};
use orca_utils::config::{APP_DB_FILE, APP_STATE_DIR};
use rusqlite::Connection;

// Re-export so downstream native crates can name `db::Connection` without
// taking a direct rusqlite dep.
pub use rusqlite::Connection as Conn;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

/// Tracks which DB paths have already had `apply_schema` + `run_pending_migrations`
/// run in this process. Subsequent opens of the same path skip both — the schema
/// only needs to be ensured once per process lifetime, and re-running on every
/// open is a hot-path cost (CREATE TABLE IF NOT EXISTS × ~30 tables + a
/// `user_version` probe) that adds up fast in long-running services like
/// `mcp-serve`, where every tool dispatch opens a connection.
static SCHEMA_INITIALIZED: Mutex<Option<HashSet<std::path::PathBuf>>> = Mutex::new(None);

fn ensure_schema_once(conn: &Connection, path: &Path) -> Result<()> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    {
        let mut guard = SCHEMA_INITIALIZED.lock().unwrap();
        let set = guard.get_or_insert_with(HashSet::new);
        if set.contains(&canonical) {
            return Ok(());
        }
    }
    apply_schema(conn)?;
    run_pending_migrations(conn)?;
    SCHEMA_INITIALIZED
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .insert(canonical);
    Ok(())
}

/// Forget the process-wide schema-initialized cache. Used by tests that need to
/// re-run schema setup on a fresh DB path.
#[doc(hidden)]
pub fn reset_schema_init_cache() {
    if let Ok(mut guard) = SCHEMA_INITIALIZED.lock() {
        *guard = None;
    }
}

pub(crate) fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

pub(crate) fn to_json_arr<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

pub(crate) fn to_json_obj<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginSearchTool {
    pub tool: String,
    #[serde(default = "default_search_arg")]
    pub arg: String,
    pub root: String,
}

fn default_search_arg() -> String {
    "query".to_string()
}

/// Apply the standard SQLite tuning PRAGMAs to a freshly-opened connection.
///
/// Centralized so every open path (encrypted, unencrypted, in-memory tests)
/// gets the same configuration — change once, applied everywhere.
///
/// All values here are mirrored by the compile-time `SQLITE_DEFAULT_*` defines
/// in `.cargo/config.toml::LIBSQLITE3_FLAGS`. The runtime PRAGMAs make the
/// configuration explicit on every connection (and survive a future build
/// without those defines), while the compile-time defaults catch any code path
/// that forgets to call this helper.
fn apply_tuning_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        // synchronous=NORMAL is the standard WAL pairing — full fsync only on
        // checkpoint, not every commit. Safe against corruption; can lose the
        // very last commit on power loss (acceptable for an app db).
        //
        // cache_size negative => kibibytes; -65536 = 64 MiB per-conn page cache.
        // mmap_size 256 MiB lets reads bypass the page cache entirely on 64-bit.
        // temp_store=MEMORY keeps temp tables/indices off disk.
        // busy_timeout=5000 reduces SQLITE_BUSY under contention.
        // wal_autocheckpoint=1000 keeps the -wal file from growing unbounded
        //   (default 1000 pages = ~4 MiB at 4K pages — fine).
        "
        PRAGMA journal_mode      = WAL;
        PRAGMA foreign_keys      = ON;
        PRAGMA synchronous       = NORMAL;
        PRAGMA cache_size        = -65536;
        PRAGMA mmap_size         = 268435456;
        PRAGMA temp_store        = MEMORY;
        PRAGMA busy_timeout      = 5000;
        PRAGMA wal_autocheckpoint = 1000;
        ",
    )
    .context("failed to apply tuning pragmas")?;
    Ok(())
}

/// SQLCipher-specific tuning. MUST be called BEFORE `PRAGMA key` — these
/// settings affect how the key is derived and how pages are protected, and
/// SQLCipher locks them in once the key is set.
fn apply_cipher_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        // kdf_iter=64000: PBKDF2 iterations dropped from default 256000.
        //   Cuts db-open latency by ~150 ms. Safe with our 256-bit random key
        //   (loaded from OS keychain) — KDF iterations only matter against
        //   weak passwords, and our key has 256 bits of entropy.
        //
        // cipher_memory_security=OFF: skip per-page zero-on-free.
        //   ~5-15% faster reads. Tradeoff: plaintext db pages can linger in
        //   process heap until overwritten naturally. Acceptable given that
        //   the host process is already trusted with the encryption key.
        "
        PRAGMA cipher_default_kdf_iter      = 64000;
        PRAGMA kdf_iter                     = 64000;
        PRAGMA cipher_memory_security       = OFF;
        ",
    )
    .context("failed to apply SQLCipher tuning pragmas")?;
    Ok(())
}

/// Open (or create) the encrypted orca database.
///
/// Key is loaded from the OS keychain on first call; generated and stored if not found.
/// The database file lives at `~/.orca/orca.db` by default.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path).context("failed to open database")?;

    // Cipher tuning MUST come before PRAGMA key — kdf_iter and
    // cipher_memory_security affect how the key is processed.
    apply_cipher_pragmas(&conn)?;

    // Load or generate the 32-byte encryption key
    let key_hex = load_or_create_key()?;
    // SQLCipher hex key syntax: x'...' — bypasses PBKDF2 and uses the raw key directly
    conn.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\";"))
        .context("failed to apply SQLCipher key")?;

    // Verify the key works (SQLCipher returns an error on wrong key when first accessing data)
    conn.execute_batch("PRAGMA user_version;")
        .context("database key rejected — key mismatch or corrupted database")?;

    apply_tuning_pragmas(&conn)?;

    ensure_schema_once(&conn, path)?;

    Ok(conn)
}

// Task-local DB path override — flows with the tokio task tree, surviving
// `.await` points, `tokio::spawn`, and worker-thread moves alike. This is
// the primary, robust override mechanism. Use `with_db_path(path, fut)` to
// scope a future.
//
// Why task_local and not thread_local: handlers in axum can await mid-request,
// and the multi-threaded runtime is free to resume them on a different worker.
// A thread_local set on the test thread is invisible there → silent fallback
// to `~/.orca/orca.db`, which on a clean machine doesn't exist → 500s.
tokio::task_local! {
    static TASK_DB_PATH: std::path::PathBuf;
}

// Legacy thread-local override — kept as a fallback for tests written before
// the task-local existed. New code should use `with_db_path`. Removal is
// blocked on migrating ~20 call sites in tests/plugin_host.rs.
thread_local! {
    static THREAD_DB_PATH: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `fut` with `path` as the active DB-path override. Every `open_default()`
/// call inside the future (and inside any task spawned from it) sees `path`.
/// Used by integration tests to isolate against an ephemeral SQLite file.
pub fn with_db_path<F>(
    path: std::path::PathBuf,
    fut: F,
) -> tokio::task::futures::TaskLocalFuture<std::path::PathBuf, F>
where
    F: std::future::Future,
{
    TASK_DB_PATH.scope(path, fut)
}

/// Legacy: set a per-thread DB path override. Prefer `with_db_path` — this
/// breaks the moment a handler awaits and resumes on another worker thread.
pub fn set_thread_db_path(path: Option<&str>) {
    THREAD_DB_PATH.with(|p| *p.borrow_mut() = path.map(|s| s.to_string()));
}

/// Open orca database using the default path (`~/.orca/orca.db`).
///
/// Resolution order:
///   1. Task-local override set by `with_db_path` (preferred — async-safe).
///   2. Thread-local override set by `set_thread_db_path` (legacy fallback).
///   3. `ORCA_DB_PATH` env var (CI / scripts).
///   4. `~/.orca/orca.db` (encrypted, production).
pub fn open_default() -> Result<Connection> {
    if let Ok(path) = TASK_DB_PATH.try_with(|p| p.clone()) {
        return open_unencrypted(&path);
    }
    if let Some(path) = THREAD_DB_PATH.with(|p| p.borrow().clone()) {
        return open_unencrypted(std::path::Path::new(&path));
    }
    if let Ok(path) = std::env::var("ORCA_DB_PATH") {
        return open_unencrypted(std::path::Path::new(&path));
    }
    let home = dirs::home_dir().context("no home dir")?;
    let path = home.join(APP_STATE_DIR).join(APP_DB_FILE);
    open(&path)
}

/// Open an unencrypted SQLite database (used for testing via `ORCA_DB_PATH`).
pub fn open_unencrypted(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).context("failed to open unencrypted database")?;
    apply_tuning_pragmas(&conn)?;
    ensure_schema_once(&conn, path)?;
    Ok(conn)
}

// ── Migrations ───────────────────────────────────────────────────────────────

/// Direction to migrate: one step up or one step down.
pub enum MigrateDirection {
    Up,
    Down,
}

/// One discovered migration on disk — a pair of `.up.sql` / `.down.sql` files
/// in `projects/db/migrations/`, embedded into the binary via `include_dir!`.
///
/// File naming: `<14-digit-YYYYMMDDHHMMSS>__<slug>.up.sql` (+ `.down.sql`).
/// Slugs are descriptive; the timestamp is the canonical ordering key and
/// the value stored in `schema_migrations.version`.
#[derive(Debug, Clone)]
struct Migration {
    version: i64,
    slug: String,
    up: String,
    down: Option<String>,
}

static MIGRATION_DIR: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/migrations");

/// Walk the embedded `migrations/` directory and produce sorted Migration
/// entries. Cheap — called once per process via `discover_migrations()`.
fn discover_migrations_inner() -> Vec<Migration> {
    use std::collections::HashMap;
    // Group files by `<version>__<slug>` stem; each may contribute .up.sql
    // and/or .down.sql.
    let mut groups: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for f in MIGRATION_DIR.files() {
        let name = f.path().file_name().and_then(|s| s.to_str()).unwrap_or("");
        let (stem, kind) = if let Some(s) = name.strip_suffix(".up.sql") {
            (s.to_string(), "up")
        } else if let Some(s) = name.strip_suffix(".down.sql") {
            (s.to_string(), "down")
        } else {
            continue;
        };
        let body = f.contents_utf8().map(|s| s.to_string()).unwrap_or_default();
        let entry = groups.entry(stem).or_insert((None, None));
        match kind {
            "up" => entry.0 = Some(body),
            "down" => entry.1 = Some(body),
            _ => {}
        }
    }

    let mut out: Vec<Migration> = groups
        .into_iter()
        .filter_map(|(stem, (up, down))| {
            // Stem format: `<14-digit-ts>__<slug>`. The version is the
            // numeric timestamp; the slug is everything after `__`.
            let (ts, slug) = stem.split_once("__")?;
            let version: i64 = ts.parse().ok()?;
            Some(Migration {
                version,
                slug: slug.to_string(),
                up: up?,
                down,
            })
        })
        .collect();
    out.sort_by_key(|m| m.version);
    out
}

fn discover_migrations() -> &'static [Migration] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<Migration>> = OnceLock::new();
    CACHE.get_or_init(discover_migrations_inner)
}

/// Ensure the `schema_migrations` tracking table exists, and bootstrap from
/// the legacy `PRAGMA user_version` scheme on first run.
///
/// Pre-2026-05-13 the runner stored the highest applied version in
/// `user_version` (a u32). The squash baseline left existing DBs at v26.
/// On first encounter with this code, we create `schema_migrations` and
/// (if user_version > 0) seed a marker row at version 0 representing
/// "everything in apply_schema is already applied" — that way newly added
/// timestamp-versioned migrations all run, and we never re-attempt v1..v26.
fn ensure_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            slug       TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        );",
    )?;
    // One-time bootstrap from legacy user_version. user_version=0 on fresh
    // DBs (no bootstrap row needed); >0 means we're upgrading from the
    // squash-baseline regime and have to stamp the table to skip v1..v26.
    let already_seeded: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = 0)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    let legacy: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if !already_seeded && legacy > 0 {
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO schema_migrations (version, slug, applied_at) VALUES (0, ?1, ?2)",
            rusqlite::params![format!("baseline_user_version_{legacy}"), now],
        )?;
        // Zero out the legacy pragma so we don't re-bootstrap on next open.
        conn.execute_batch("PRAGMA user_version = 0;")?;
    }
    Ok(())
}

/// Highest applied migration version, or 0 if none have been applied.
pub fn schema_version(conn: &Connection) -> Result<i64> {
    ensure_migrations_table(conn)?;
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(v)
}

/// Total number of migrations defined on disk (applied + pending combined).
pub fn migration_count() -> usize {
    discover_migrations().len()
}

/// Number of migrations recorded as applied (excludes the synthetic
/// baseline row at version=0).
pub fn applied_count(conn: &Connection) -> Result<u32> {
    ensure_migrations_table(conn)?;
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version > 0",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(n.max(0) as u32)
}

/// Run pending up-migrations automatically after `apply_schema`. Idempotent.
fn run_pending_migrations(conn: &Connection) -> Result<()> {
    migrate(conn, MigrateDirection::Up, usize::MAX)?;
    Ok(())
}

/// Apply or revert migrations.
///
/// - `Up` with `steps = usize::MAX` runs all pending migrations (startup default).
/// - `Up` with `steps = 1` applies the next pending migration.
/// - `Down` with `steps = 1` reverts the most recently applied migration.
///
/// Returns the new schema version.
pub fn migrate(conn: &Connection, direction: MigrateDirection, steps: usize) -> Result<i64> {
    ensure_migrations_table(conn)?;

    let applied: std::collections::HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let all = discover_migrations();

    match direction {
        MigrateDirection::Up => {
            let pending: Vec<&Migration> = all
                .iter()
                .filter(|m| !applied.contains(&m.version))
                .take(steps)
                .collect();

            if pending.is_empty() {
                tracing::debug!("nothing to migrate");
            }

            for m in pending {
                if let Err(e) = conn.execute_batch(&m.up) {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("duplicate column") || msg.contains("already exists") {
                        // Column/table already present — idempotent, mark as done.
                    } else {
                        return Err(anyhow::anyhow!(
                            "migration {} ({}) failed: {e}",
                            m.version,
                            m.slug
                        ));
                    }
                }
                let now = chrono::Utc::now().timestamp();
                conn.execute(
                    "INSERT INTO schema_migrations (version, slug, applied_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![m.version, m.slug, now],
                )?;
                eprintln!("  ↑  {} {}", m.version, m.slug);
            }
        }

        MigrateDirection::Down => {
            // Roll back the most-recently-applied versions first. Skip the
            // synthetic baseline row at version=0 — it represents the
            // pre-migration-system schema and isn't reversible.
            let mut applied_sorted: Vec<i64> = applied.iter().copied().filter(|v| *v > 0).collect();
            applied_sorted.sort_unstable();
            applied_sorted.reverse();

            for v in applied_sorted.into_iter().take(steps) {
                let m = match all.iter().find(|m| m.version == v) {
                    Some(m) => m,
                    None => {
                        eprintln!("  ~  {v}: no on-disk migration found — clearing tracking row");
                        conn.execute(
                            "DELETE FROM schema_migrations WHERE version = ?1",
                            rusqlite::params![v],
                        )?;
                        continue;
                    }
                };
                match &m.down {
                    Some(sql) => {
                        conn.execute_batch(sql)?;
                        eprintln!("  ↓  {} {}", m.version, m.slug);
                    }
                    None => {
                        eprintln!(
                            "  ~  {} {}: no down migration — clearing tracking row only",
                            m.version, m.slug
                        );
                    }
                }
                conn.execute(
                    "DELETE FROM schema_migrations WHERE version = ?1",
                    rusqlite::params![m.version],
                )?;
            }
        }
    }

    schema_version(conn)
}

// ── Schema ───────────────────────────────────────────────────────────────────

fn apply_schema(conn: &Connection) -> Result<()> {
    // Consolidated v1 baseline (2026-05-12 squash) — folds the legacy
    // migrations 1..26 into a single CREATE-IF-NOT-EXISTS bundle. Future
    // schema changes are appended to MIGRATIONS as version=1 onward.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS learning_progress (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS session_events (
            id          TEXT PRIMARY KEY,
            session     TEXT NOT NULL,
            project     TEXT,
            timestamp   TEXT NOT NULL,
            role        TEXT,
            agent       TEXT,
            content     TEXT,
            important   INTEGER NOT NULL DEFAULT 0,
            tags        TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_se_session   ON session_events(session);
        CREATE INDEX IF NOT EXISTS idx_se_project   ON session_events(project);
        CREATE INDEX IF NOT EXISTS idx_se_important ON session_events(important);
        CREATE INDEX IF NOT EXISTS idx_se_timestamp ON session_events(timestamp);

        CREATE VIRTUAL TABLE IF NOT EXISTS session_events_fts
            USING fts5(id UNINDEXED, content, content='session_events', content_rowid='rowid');

        CREATE TRIGGER IF NOT EXISTS se_fts_insert
            AFTER INSERT ON session_events BEGIN
                INSERT INTO session_events_fts(rowid, id, content)
                VALUES (new.rowid, new.id, new.content);
            END;

        CREATE TRIGGER IF NOT EXISTS se_fts_delete
            AFTER DELETE ON session_events BEGIN
                INSERT INTO session_events_fts(session_events_fts, rowid, id, content)
                VALUES ('delete', old.rowid, old.id, old.content);
            END;

        CREATE TABLE IF NOT EXISTS mcp_servers (
            name       TEXT PRIMARY KEY,
            command    TEXT NOT NULL,
            args       TEXT NOT NULL DEFAULT '[]',
            env        TEXT NOT NULL DEFAULT '{}',
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS mcp_tool_mappings (
            orca_tool       TEXT PRIMARY KEY,
            mcp_name        TEXT NOT NULL REFERENCES mcp_servers(name) ON DELETE CASCADE,
            external_tool   TEXT NOT NULL,
            match_type      TEXT NOT NULL DEFAULT 'explicit',
            confidence      REAL,
            enabled         INTEGER NOT NULL DEFAULT 1,
            created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            verified_at     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_mtm_mcp     ON mcp_tool_mappings(mcp_name);
        CREATE INDEX IF NOT EXISTS idx_mtm_enabled ON mcp_tool_mappings(enabled);

        CREATE TABLE IF NOT EXISTS schema_databases (
            name         TEXT PRIMARY KEY,
            host         TEXT,
            port         INTEGER,
            user         TEXT NOT NULL DEFAULT '',
            password     TEXT NOT NULL DEFAULT '',
            database     TEXT NOT NULL DEFAULT '',
            container    TEXT,
            domains_file TEXT,
            driver       TEXT NOT NULL DEFAULT 'mysql',
            enabled      INTEGER NOT NULL DEFAULT 1,
            created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS openapi_specs (
            name        TEXT PRIMARY KEY,
            url         TEXT,
            source_mcp  TEXT,
            spec_json   TEXT,
            cached_at   TEXT,
            enabled     INTEGER NOT NULL DEFAULT 1,
            created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS docker_runtimes (
            name        TEXT PRIMARY KEY,
            socket_path TEXT,
            host        TEXT,
            url         TEXT,
            enabled     INTEGER NOT NULL DEFAULT 1,
            created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS proxmox_endpoints (
            name         TEXT PRIMARY KEY,
            base_url     TEXT NOT NULL,
            token_id     TEXT NOT NULL,
            token_secret TEXT NOT NULL,
            insecure     INTEGER NOT NULL DEFAULT 0,
            enabled      INTEGER NOT NULL DEFAULT 1,
            created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS homeassistant_endpoints (
            name       TEXT PRIMARY KEY,
            base_url   TEXT NOT NULL,
            token      TEXT NOT NULL,
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS plugins (
            id                TEXT PRIMARY KEY,
            manifest_path     TEXT NOT NULL,
            tier              TEXT NOT NULL DEFAULT 'personal',
            mcp_command       TEXT,
            mcp_args          TEXT NOT NULL DEFAULT '[]',
            mcp_env           TEXT NOT NULL DEFAULT '{}',
            mcp_url           TEXT,
            mcp_token_env     TEXT,
            context_injection TEXT NOT NULL DEFAULT 'minimal',
            command_map       TEXT NOT NULL DEFAULT '{}',
            mode              TEXT NOT NULL DEFAULT 'orca',
            nav_links         TEXT NOT NULL DEFAULT '[]',
            search_tools      TEXT NOT NULL DEFAULT '[]',
            specs_dir         TEXT,
            enabled           INTEGER NOT NULL DEFAULT 1,
            created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS plugin_credentials (
            plugin_id  TEXT NOT NULL,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            synced_at  TEXT,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (plugin_id, key)
        );

        CREATE TABLE IF NOT EXISTS plugin_data (
            plugin_id  TEXT NOT NULL,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (plugin_id, key)
        );

        CREATE TABLE IF NOT EXISTS plugin_deps (
            parent_id  TEXT NOT NULL,
            dep_id     TEXT NOT NULL,
            PRIMARY KEY (parent_id, dep_id)
        );

        CREATE TABLE IF NOT EXISTS plugin_types (
            plugin_id      TEXT NOT NULL,
            type_name      TEXT NOT NULL,
            fq_type_id     TEXT NOT NULL UNIQUE,
            schema_version TEXT NOT NULL,
            schema_json    TEXT NOT NULL,
            sensitivity    TEXT NOT NULL DEFAULT 'general'
                           CHECK (sensitivity IN ('general','sensitive')),
            declared_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (plugin_id, type_name)
        );
        CREATE INDEX IF NOT EXISTS idx_plugin_types_fq ON plugin_types(fq_type_id);

        CREATE TABLE IF NOT EXISTS plugin_tools (
            plugin_id        TEXT NOT NULL,
            name             TEXT NOT NULL,
            fq_name          TEXT NOT NULL UNIQUE,
            description      TEXT NOT NULL,
            input_schema     TEXT NOT NULL,
            sensitivity      TEXT NOT NULL DEFAULT 'general'
                             CHECK (sensitivity IN ('general','sensitive')),
            declared_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (plugin_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_plugin_tools_fq ON plugin_tools(fq_name);

        CREATE TABLE IF NOT EXISTS plugin_installs (
            system_id         TEXT NOT NULL,
            plugin_id         TEXT NOT NULL,
            channel           TEXT NOT NULL DEFAULT 'latest'
                              CHECK (channel IN ('latest','latest-rc','locked')),
            locked_version    TEXT,
            desired_version   TEXT,
            installed_version TEXT,
            installed_at      TEXT,
            updated_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (system_id, plugin_id),
            CHECK (channel != 'locked' OR locked_version IS NOT NULL)
        );
        CREATE INDEX IF NOT EXISTS idx_plugin_installs_plugin ON plugin_installs(plugin_id);

        CREATE TABLE IF NOT EXISTS oauth_tokens (
            service       TEXT PRIMARY KEY,
            access_token  TEXT NOT NULL,
            refresh_token TEXT,
            expires_at    TEXT,
            updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS llm_providers (
            name       TEXT PRIMARY KEY,
            url        TEXT NOT NULL,
            kind       TEXT NOT NULL DEFAULT 'lmstudio',
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS doc_roots (
            name        TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            description TEXT,
            enabled     INTEGER NOT NULL DEFAULT 1,
            created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        INSERT OR IGNORE INTO doc_roots (name, path, description) VALUES
            ('rebuy',    '~/code/rebuy',    'Rebuy monorepo'),
            ('orca',     '~/code/orca',     'Orca codebase'),
            ('bardbase', '~/code/bardbase', 'Bardbase'),
            ('homepage', '~/code/homepage', 'Homepage'),
            ('meerkat',  '~/code/meerkat',  'Meerkat');

        CREATE TABLE IF NOT EXISTS doc_ignore_patterns (
            pattern    TEXT PRIMARY KEY,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        INSERT OR IGNORE INTO doc_ignore_patterns (pattern) VALUES
            ('.git'), ('node_modules'), ('target'), ('.next'), ('dist'),
            ('build'), ('vendor'), ('.trash'), ('logs'), ('memory'),
            ('plugins'), ('.turbo'), ('coverage'), ('out'), ('.cache');

        CREATE TABLE IF NOT EXISTS settings (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        INSERT OR IGNORE INTO settings (key, value) VALUES
            ('fs.allow_unrestricted', 'false'),
            ('ui.enabled',            'true');

        CREATE TABLE IF NOT EXISTS profiles (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            owner_user_id   TEXT NOT NULL,
            description     TEXT,
            created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            UNIQUE (owner_user_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_profiles_owner ON profiles(owner_user_id);

        CREATE TABLE IF NOT EXISTS profile_shares (
            profile_id  TEXT NOT NULL,
            user_id     TEXT NOT NULL,
            role        TEXT NOT NULL CHECK (role IN ('viewer','collaborator')),
            shared_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (profile_id, user_id),
            FOREIGN KEY (profile_id) REFERENCES profiles(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_profile_shares_user ON profile_shares(user_id);

        CREATE TABLE IF NOT EXISTS user_active_profile (
            user_id     TEXT PRIMARY KEY,
            profile_id  TEXT NOT NULL,
            updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            FOREIGN KEY (profile_id) REFERENCES profiles(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS profile_credentials (
            profile_id  TEXT NOT NULL,
            key         TEXT NOT NULL,
            value       TEXT NOT NULL,
            updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (profile_id, key),
            FOREIGN KEY (profile_id) REFERENCES profiles(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS secrets (
            name        TEXT PRIMARY KEY,
            backend     TEXT NOT NULL,
            ref_path    TEXT NOT NULL DEFAULT '',
            description TEXT,
            created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        -- pod_discovery: mDNS / manual-probe seen peers, keyed by ed25519
        -- bootstrap pubkey fingerprint (stable across restarts and IP changes).
        -- state = 'unclaimed' (no mesh CA) or 'pod:<pod_id>' (member of a pod).
        -- can_invite = 1 iff that peer advertises it has the mesh CA private key
        -- AND has self_secure=true. Auto-offer scheduler only targets state=unclaimed.
        CREATE TABLE IF NOT EXISTS pod_discovery (
            pubkey_fp     TEXT PRIMARY KEY,
            peer_id       TEXT,
            hostname      TEXT NOT NULL,
            addr          TEXT NOT NULL,
            port          INTEGER NOT NULL,
            state         TEXT NOT NULL,
            can_invite    INTEGER NOT NULL DEFAULT 0,
            first_seen_at INTEGER NOT NULL,
            last_seen_at  INTEGER NOT NULL
        );

        -- pod_pending_offers: outstanding pairing offers in either direction.
        -- direction='out' rows are offers WE pushed (inviter side); 'in' rows
        -- are offers WE received and are waiting for the user to `pod accept`
        -- with the matching code. code_hash is sha256(code) so the raw code
        -- only lives in human memory + the wire blob.
        CREATE TABLE IF NOT EXISTS pod_pending_offers (
            offer_id        TEXT PRIMARY KEY,
            direction       TEXT NOT NULL CHECK (direction IN ('in','out')),
            peer_pubkey_fp  TEXT NOT NULL,
            peer_hostname   TEXT NOT NULL,
            peer_addr       TEXT NOT NULL,
            peer_port       INTEGER NOT NULL,
            code_hash       TEXT NOT NULL,
            mesh_ca_cert_pem TEXT,
            inviter_peer_id TEXT,
            pod_id          TEXT,
            expires_at      INTEGER NOT NULL,
            created_at      INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pod_pending_offers_fp
            ON pod_pending_offers (peer_pubkey_fp, direction);

        -- pod_peers: paired members of the pod. port is the address of the
        -- peer's pod surface; departed_at marks a peer that ran `pod leave`
        -- and is no longer trusted until re-paired.
        CREATE TABLE IF NOT EXISTS pod_peers (
            peer_id       TEXT PRIMARY KEY,
            peer_hostname TEXT NOT NULL,
            peer_addr     TEXT NOT NULL DEFAULT '',
            peer_port     INTEGER NOT NULL DEFAULT 12002,
            pubkey_fp     TEXT,
            ca_cert_pem   TEXT NOT NULL,
            first_seen_at INTEGER NOT NULL,
            last_seen_at  INTEGER NOT NULL,
            departed_at   INTEGER
        );

        CREATE TABLE IF NOT EXISTS pod_trust (
            peer_id      TEXT PRIMARY KEY REFERENCES pod_peers(peer_id) ON DELETE CASCADE,
            local_secure INTEGER NOT NULL DEFAULT 0,
            peer_secure  INTEGER NOT NULL DEFAULT 0,
            set_at       INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS pod_self (
            id                       INTEGER PRIMARY KEY CHECK (id = 1),
            self_secure              INTEGER NOT NULL DEFAULT 0,
            pod_id                   TEXT,
            ca_previous_expires_at   INTEGER,
            set_at                   INTEGER NOT NULL
        );

        -- Config store: typed, host-owned rows that drive the scheduler,
        -- services, backups, NFS watches, chown sweeps, etc.
        -- See docs/planned/orca-v1-scope.md §3.1.
        CREATE TABLE IF NOT EXISTS config_rows (
            id          TEXT PRIMARY KEY,
            host_owner  TEXT NOT NULL,
            noun        TEXT NOT NULL,
            name        TEXT NOT NULL,
            json        TEXT NOT NULL,
            is_replica  INTEGER NOT NULL DEFAULT 0,
            updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            updated_by  TEXT NOT NULL DEFAULT 'local',
            UNIQUE (noun, name, host_owner)
        );
        CREATE INDEX IF NOT EXISTS idx_config_rows_noun  ON config_rows(noun);
        CREATE INDEX IF NOT EXISTS idx_config_rows_owner ON config_rows(host_owner);

        CREATE TABLE IF NOT EXISTS config_history (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            row_id      TEXT NOT NULL,
            prior_json  TEXT NOT NULL,
            changed_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            changed_by  TEXT NOT NULL DEFAULT 'local'
        );
        CREATE INDEX IF NOT EXISTS idx_config_history_row ON config_history(row_id);

        CREATE TABLE IF NOT EXISTS config_schemas (
            noun             TEXT PRIMARY KEY,
            schema_json      TEXT NOT NULL,
            sensitive_fields TEXT NOT NULL DEFAULT '[]',
            registered_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        -- Scheduler run history — one row per periodic-loop tick.
        -- See docs/planned/orca-v1-scope.md §3.4.
        CREATE TABLE IF NOT EXISTS scheduler_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            job_name    TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            ok          INTEGER NOT NULL,
            error       TEXT,
            duration_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_scheduler_runs_job_started
            ON scheduler_runs(job_name, started_at DESC);
        ",
    )?;
    Ok(())
}

// ── Key management ───────────────────────────────────────────────────────────

/// Load the DB encryption key from `~/.orca/.db_key`, generating it on first run.
///
/// The key file is the backup unit alongside orca.db — copy both to restore.
/// Never regenerate silently: if the file exists but is unreadable/corrupt, bail
/// so the user knows they need to restore the key rather than destroying their data.
fn load_or_create_key() -> Result<String> {
    let home = dirs::home_dir().context("no home dir")?;
    let key_path = home.join(APP_STATE_DIR).join(".db_key");

    if key_path.exists() {
        let raw = std::fs::read_to_string(&key_path)
            .context("failed to read ~/.orca/.db_key — restore from backup or run `orca db reset` to wipe and start fresh")?;
        let key = raw.trim().to_string();
        anyhow::ensure!(
            key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()),
            "~/.orca/.db_key is corrupt (expected 64 hex chars) — restore from backup"
        );
        return Ok(key);
    }

    // First run: generate key, write with restricted permissions.
    // Use getrandom directly — rand 0.10 reorganized its OS RNG surface and
    // for a one-shot 32-byte crypto key we don't need a full RNG abstraction.
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("OS RNG failure generating db key: {e}"))?;
    let hex: String = bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    });

    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&key_path, &hex).context("failed to write ~/.orca/.db_key")?;

    // Restrict to owner-read/write only (0600)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }

    tracing::info!(
        "generated new DB encryption key at ~/.orca/.db_key — back this up alongside orca.db"
    );
    Ok(hex)
}

// ── Learning progress ─────────────────────────────────────────────────────────

/// Retrieve the last saved learning page, if any.
pub fn get_learning_progress(conn: &Connection) -> Result<Option<String>> {
    let page = conn
        .query_row(
            "SELECT value FROM learning_progress WHERE key = 'current_page'",
            [],
            |row| row.get(0),
        )
        .ok();
    Ok(page)
}

/// Save (upsert) the current learning page.
pub fn save_learning_progress(conn: &Connection, page: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO learning_progress(key, value) VALUES('current_page', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![page],
    )?;
    Ok(())
}

// ── Write helpers ─────────────────────────────────────────────────────────────

/// Insert a session event record. Tags should be a JSON array string or empty.
#[allow(clippy::too_many_arguments)]
pub fn insert_event(
    conn: &Connection,
    id: &str,
    session: &str,
    project: Option<&str>,
    timestamp: &str,
    role: Option<&str>,
    agent: Option<&str>,
    content: Option<&str>,
    important: bool,
    tags: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO session_events
            (id, session, project, timestamp, role, agent, content, important, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            id,
            session,
            project,
            timestamp,
            role,
            agent,
            content,
            important as i32,
            tags,
        ],
    )?;
    Ok(())
}

// ── Query helpers ─────────────────────────────────────────────────────────────

pub struct EventRow {
    pub id: String,
    pub session: String,
    pub project: Option<String>,
    pub timestamp: String,
    pub role: Option<String>,
    pub agent: Option<String>,
    pub content: Option<String>,
    pub important: bool,
    pub tags: Option<String>,
}

/// Full-text search across session event content.
pub fn search_events(conn: &Connection, query: &str, limit: usize) -> Result<Vec<EventRow>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.session, e.project, e.timestamp, e.role, e.agent, e.content, e.important, e.tags
         FROM session_events e
         JOIN session_events_fts f ON f.rowid = e.rowid
         WHERE session_events_fts MATCH ?1
         ORDER BY e.timestamp DESC
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![query, limit as i64], |row| {
        Ok(EventRow {
            id: row.get(0)?,
            session: row.get(1)?,
            project: row.get(2)?,
            timestamp: row.get(3)?,
            role: row.get(4)?,
            agent: row.get(5)?,
            content: row.get(6)?,
            important: row.get::<_, i32>(7)? != 0,
            tags: row.get(8)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Retrieve all important events for a project.
pub fn important_events(conn: &Connection, project: &str, limit: usize) -> Result<Vec<EventRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, session, project, timestamp, role, agent, content, important, tags
         FROM session_events
         WHERE project = ?1 AND important = 1
         ORDER BY timestamp DESC
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![project, limit as i64], |row| {
        Ok(EventRow {
            id: row.get(0)?,
            session: row.get(1)?,
            project: row.get(2)?,
            timestamp: row.get(3)?,
            role: row.get(4)?,
            agent: row.get(5)?,
            content: row.get(6)?,
            important: row.get::<_, i32>(7)? != 0,
            tags: row.get(8)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn fs_allow_unrestricted(conn: &Connection) -> bool {
    settings::get_legacy(conn, "fs.allow_unrestricted")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Open an unencrypted in-memory database with full schema + migrations applied.
    pub fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        // In-memory dbs ignore journal_mode=WAL and mmap_size, but the rest
        // (synchronous, cache_size, temp_store, busy_timeout) all apply.
        // Calling the same helper keeps test + prod configuration aligned.
        apply_tuning_pragmas(&conn).expect("apply_tuning_pragmas");
        apply_schema(&conn).expect("apply_schema");
        run_pending_migrations(&conn).expect("migrations");
        conn
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::testing::test_conn;

    // ── Migrations ────────────────────────────────────────────────────────────

    #[test]
    fn migrations_run_to_latest() {
        let conn = test_conn();
        // After test_conn opens, every on-disk migration should be recorded.
        assert_eq!(applied_count(&conn).unwrap() as usize, migration_count());
    }

    #[test]
    fn migrate_up_idempotent_already_at_latest() {
        let conn = test_conn();
        let v_before = schema_version(&conn).unwrap();
        let applied_before = applied_count(&conn).unwrap();
        migrate(&conn, MigrateDirection::Up, usize::MAX).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), v_before);
        assert_eq!(applied_count(&conn).unwrap(), applied_before);
    }

    // ── Learning progress ─────────────────────────────────────────────────────

    #[test]
    fn learning_progress_round_trip() {
        let conn = test_conn();
        assert!(get_learning_progress(&conn).unwrap().is_none());
        save_learning_progress(&conn, "page-42").unwrap();
        assert_eq!(
            get_learning_progress(&conn).unwrap().as_deref(),
            Some("page-42")
        );
        // Upsert overwrites
        save_learning_progress(&conn, "page-99").unwrap();
        assert_eq!(
            get_learning_progress(&conn).unwrap().as_deref(),
            Some("page-99")
        );
    }

    // ── Session events ────────────────────────────────────────────────────────

    #[test]
    fn insert_and_search_event() {
        let conn = test_conn();
        insert_event(
            &conn,
            "ev-1",
            "sess-1",
            Some("orca"),
            "2026-01-01T00:00:00Z",
            Some("user"),
            Some("orca"),
            Some("hello world unique phrase"),
            false,
            None,
        )
        .unwrap();
        let results = search_events(&conn, "unique", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "ev-1");
        assert_eq!(results[0].session, "sess-1");
        assert_eq!(results[0].project.as_deref(), Some("orca"));
    }

    #[test]
    fn insert_event_ignore_duplicate_id() {
        let conn = test_conn();
        insert_event(
            &conn,
            "dup",
            "s",
            None,
            "2026-01-01T00:00:00Z",
            None,
            None,
            Some("a"),
            false,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            "dup",
            "s",
            None,
            "2026-01-01T00:00:00Z",
            None,
            None,
            Some("b"),
            false,
            None,
        )
        .unwrap();
        let results = search_events(&conn, "a", 10).unwrap();
        assert_eq!(results.len(), 1, "duplicate id should be ignored");
    }

    #[test]
    fn important_events_filters_by_project() {
        let conn = test_conn();
        insert_event(
            &conn,
            "imp-1",
            "s",
            Some("proj-a"),
            "2026-01-01T00:00:00Z",
            None,
            None,
            Some("important thing"),
            true,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            "imp-2",
            "s",
            Some("proj-b"),
            "2026-01-01T00:00:00Z",
            None,
            None,
            Some("other thing"),
            true,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            "not-imp",
            "s",
            Some("proj-a"),
            "2026-01-01T00:00:00Z",
            None,
            None,
            Some("boring"),
            false,
            None,
        )
        .unwrap();

        let results = important_events(&conn, "proj-a", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "imp-1");
        assert!(results[0].important);
    }

    // ── Settings (fs flag) ───────────────────────────────────────────────────

    #[test]
    fn fs_allow_unrestricted_seeded_false() {
        let conn = test_conn();
        // Migration 17 seeds this as 'false'
        assert!(!fs_allow_unrestricted(&conn));
        settings::set_legacy(&conn, "fs.allow_unrestricted", "true").unwrap();
        assert!(fs_allow_unrestricted(&conn));
    }
}
