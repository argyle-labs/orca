//! Encrypted SQLite database (`orca.db`) — the runtime registry for all dynamic orca config.
//!
//! `open_default()` is the standard entry point. It opens (or creates) `~/.orca/orca.db`,
//! applies the SQLCipher encryption key, runs `apply_schema` to ensure all tables exist,
//! then applies any pending schema migrations via `run_pending_migrations`.
//!
//! Adding a new registry feature means adding a table in `apply_schema`, CRUD helpers at
//! the bottom of this file, and a migration entry in `MIGRATIONS` if the table was added
//! to an already-deployed database.

pub mod api_tokens;
pub mod cache;
pub mod config_store;
// `docker` runtime registry now lives in the docker plugin via
// `plugin_toolkit::endpoint_resource!` (docker.{list,detail,create,update,
// delete}) — the daemon owns the table through the SchemaFragment inventory.
// `dockge` endpoint registry now lives in the dockge plugin via
// `plugin_toolkit::endpoint_resource!` — that macro emits the row
// struct, the CRUD module, and a SchemaFragment registration.
pub mod docs;
pub mod feature_flags;
// `home_assistant` endpoint registry now lives in the homeassistant plugin via
// `plugin_toolkit::endpoint_resource!` — that macro emits the row
// struct, the CRUD module, and a SchemaFragment registration.
pub mod host_addressing;
pub mod host_capabilities;
pub mod host_status;
pub mod llm;
pub mod maintenance;
pub mod mcp_servers;
pub mod models;
// `ntfy` endpoint registry now lives in the ntfy plugin via
// `plugin_toolkit::endpoint_resource!`.
pub mod oauth;
pub mod openapi_specs;
pub mod openapi_specs_registry;
pub mod peer_detail_state;
pub mod peer_update_state;
pub mod plugin_creds;
pub mod plugin_data;
pub mod plugin_installs;
pub mod plugin_manifest;
pub mod plugin_tables;
pub mod plugin_tools;
pub mod plugin_types;
pub mod plugins;
pub mod pod;
pub mod pool;
pub mod replicate;
pub mod replicate_engine;
pub mod schema_fragments;

// Self-alias so in-crate code and tests can name `db::…` paths just like
// downstream callers do (`db::open_unencrypted`, `db::pod::…`, etc.).
// Originally added for proc-macro emissions; the macros now target
// `::db_types::…` directly, but the alias still earns its keep as a
// uniform-path convenience inside the crate.
extern crate self as db;

pub mod ports;
pub mod profile_creds;
pub mod profiles;
// `proxmox` endpoint registry now lives in the proxmox plugin via
// `plugin_toolkit::endpoint_resource!`.
pub mod scheduler_runs;
pub mod schema;
pub mod schema_databases;
pub mod secrets;
pub mod sessions;
pub mod settings;
pub mod startup;
pub mod tool_mappings;
pub mod users;

use anyhow::{Context, Result};
use contract::config::APP_DB_FILE;
use rusqlite::Connection;

// Re-export so downstream native crates can name `db::Connection` without
// taking a direct rusqlite dep.
pub use rusqlite::Connection as Conn;
use std::collections::HashSet;
use std::fmt::Write as _;
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
        // journal_mode=DELETE (rollback journal), NOT WAL. SQLCipher + WAL +
        // multiple connections in ONE process reliably short-reads the shared
        // wal-index (-shm) and fails with SQLITE_IOERR_SHORT_READ (522): every
        // fresh `open_default()` connection opened while the long-lived
        // `db::pool` connection is active floods 522s seconds after boot
        // (observed: 244 host_status_writer + 49 replicate errors in a few
        // seconds), while an external single-connection process reads the same
        // file fine. A rollback journal removes the -wal/-shm coordination
        // entirely and eliminates the whole failure class for every open path
        // at once. The daemon's real concurrency is low and `db::pool` already
        // serializes access behind a Mutex, so WAL bought nothing here but the
        // race.
        //
        // synchronous=FULL is the safe pairing for a rollback journal (NORMAL's
        // no-corruption-on-power-loss guarantee is WAL-specific); the db is a
        // few MB so the extra fsync cost is negligible.
        //
        // cache_size negative => kibibytes; -65536 = 64 MiB per-conn page cache.
        // mmap_size=0: memory-mapped I/O is DISABLED. SQLCipher decrypts every
        // page into the pager cache and cannot serve pages through the memory
        // map, so a non-zero mmap_size on the encrypted connection is a no-op at
        // best and a footgun at worst. (This is the only db we open; the
        // unencrypted test path is in-memory and ignores mmap regardless.)
        // temp_store=MEMORY keeps temp tables/indices off disk.
        // busy_timeout=5000 reduces SQLITE_BUSY under contention.
        "
        PRAGMA journal_mode      = DELETE;
        PRAGMA foreign_keys      = ON;
        PRAGMA synchronous       = FULL;
        PRAGMA cache_size        = -65536;
        PRAGMA mmap_size         = 0;
        PRAGMA temp_store        = MEMORY;
        PRAGMA busy_timeout      = 5000;
        PRAGMA auto_vacuum       = INCREMENTAL;
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

/// Turn a failed post-`PRAGMA key` probe into an error whose message reflects
/// the ACTUAL cause. A wrong key or non-database file surfaces as
/// `SQLITE_NOTADB (26)`; anything else — most importantly the I/O errors like
/// `SQLITE_IOERR_SHORT_READ (522)` seen under connection contention — is passed
/// through verbatim so it is not misdiagnosed as a key mismatch.
fn classify_key_check_error(e: rusqlite::Error) -> anyhow::Error {
    if let rusqlite::Error::SqliteFailure(err, _) = &e
        && err.code == rusqlite::ErrorCode::NotADatabase
    {
        return anyhow::Error::new(e)
            .context("database key rejected — key mismatch or non-database file");
    }
    anyhow::Error::new(e).context("failed to read database after applying key")
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

    // Verify the key works (SQLCipher returns an error on wrong key when first
    // accessing data). Distinguish a genuine key/format rejection from an I/O
    // error (e.g. SQLITE_IOERR_SHORT_READ 522): masking every failure as
    // "key rejected" sent live debugging down the wrong path for hours.
    if let Err(e) = conn.execute_batch("PRAGMA user_version;") {
        return Err(classify_key_check_error(e));
    }

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

/// Run `f` with the thread-local DB-path override pinned to `path`, restoring
/// the previous value on return (or unwind). Use this from synchronous tests
/// of tool bodies that call `open_default()` — it removes the
/// `set_thread_db_path(Some)…set_thread_db_path(None)` book-keeping and is
/// panic-safe, so a failing assertion never leaks the override into the next
/// test on the same thread.
///
/// For `async fn` tests, keep using `with_db_path`, which uses a task-local
/// that survives executor thread hops.
pub fn with_thread_db_path<F, R>(path: &std::path::Path, f: F) -> R
where
    F: FnOnce() -> R,
{
    struct Guard(Option<String>);
    impl Drop for Guard {
        fn drop(&mut self) {
            THREAD_DB_PATH.with(|p| *p.borrow_mut() = self.0.take());
        }
    }
    let prev = THREAD_DB_PATH.with(|p| p.borrow().clone());
    THREAD_DB_PATH.with(|p| *p.borrow_mut() = Some(path.to_string_lossy().into_owned()));
    let _guard = Guard(prev);
    f()
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
    // Canonical resolver: honors $ORCA_HOME (was dirs::home_dir(), which ignored
    // it — so a custom $ORCA_HOME left the DB behind in $HOME/.orca).
    let path = contract::config::state_dir()?.join(APP_DB_FILE);
    open(&path)
}

/// Ensure the on-disk database uses a rollback journal (DELETE), not WAL.
///
/// `journal_mode` is a PERSISTENT property of the database file. Converting an
/// existing WAL database to DELETE requires exclusive access: if any other
/// connection already holds the WAL open, `PRAGMA journal_mode=DELETE` silently
/// leaves it in WAL (the pragma returns "wal" and nothing changes). Per-open
/// tuning in [`apply_tuning_pragmas`] therefore CANNOT be relied on to convert
/// an existing WAL db at daemon boot — many connections open near-simultaneously
/// and the conversion loses the race, leaving the file in WAL and every fresh
/// open failing with SQLITE_IOERR_SHORT_READ (522).
///
/// Call this ONCE at daemon startup, before the pool or any background task
/// opens a connection, so the conversion runs uncontested. Idempotent: a no-op
/// on an already-DELETE or freshly-created database. Verifies the result and
/// errors if the file is still WAL (which means the call-ordering contract was
/// violated — something opened the db first).
pub fn ensure_rollback_journal() -> Result<()> {
    let conn = open_default()?;
    // Flush any pending WAL into the main db, then convert. `query_row` reads the
    // mode the pragma returns so we can verify the conversion actually took.
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    let mode: String = conn
        .query_row("PRAGMA journal_mode = DELETE;", [], |r| r.get(0))
        .context("convert journal_mode to DELETE")?;
    if !mode.eq_ignore_ascii_case("delete") {
        anyhow::bail!(
            "journal_mode is still {mode:?} after DELETE conversion — the db was \
             already open by another connection; ensure_rollback_journal must run \
             before the pool and any background task opens the db"
        );
    }
    Ok(())
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
                tracing::trace!("nothing to migrate");
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
    // Consolidated v2 baseline (2026-05-20 squash) — folds the previous v1
    // baseline + every timestamp-versioned migration that landed between
    // 2026-05-13 and 2026-05-17 into a single CREATE-IF-NOT-EXISTS bundle.
    // Future schema changes go back into `projects/db/migrations/` as new
    // timestamp-versioned `.up.sql` / `.down.sql` pairs.
    //
    // Why the squash: keeping 13 pre-prod migrations around makes every
    // fresh install replay them in order, and locks the column shapes for
    // pod_peers + pod_self into a sequence of ALTER TABLEs rather than the
    // intended final CREATE. Window closes the moment a non-Scott peer
    // joins the mesh (= the first time we have to honour replay history).
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

        -- docker_runtimes, proxmox_endpoints, homeassistant_endpoints,
        -- ntfy_endpoints, and dockge_endpoints all live in their respective
        -- plugins via
        -- `plugin_toolkit::endpoint_resource!`, registered through the
        -- SchemaFragment inventory (applied below).

        CREATE TABLE IF NOT EXISTS plugins (
            id                TEXT PRIMARY KEY,
            manifest_path     TEXT NOT NULL,
            tier              TEXT NOT NULL DEFAULT 'personal',
            -- Transport columns (mcp_command/mcp_args/mcp_env/mcp_url/mcp_token_env)
            -- and `mode` are dropped by migrations:
            --   20260530130000__plugins_drop_mode
            --   20260530140000__plugins_drop_mcp_transport
            -- Kept in CREATE TABLE so fresh + migrated dbs converge on the same
            -- final schema after migrations run.
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
            plugin_id        TEXT NOT NULL,
            plugin_namespace TEXT NOT NULL DEFAULT '',
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
            plugin_namespace TEXT NOT NULL DEFAULT '',
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

        CREATE TABLE IF NOT EXISTS models (
            id         TEXT PRIMARY KEY,
            provider   TEXT NOT NULL,
            endpoint   TEXT,
            model_name TEXT NOT NULL,
            is_default INTEGER NOT NULL DEFAULT 0,
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        CREATE UNIQUE INDEX IF NOT EXISTS models_one_default
            ON models(is_default) WHERE is_default = 1;

        CREATE TABLE IF NOT EXISTS doc_roots (
            name        TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            description TEXT,
            enabled     INTEGER NOT NULL DEFAULT 1,
            created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        -- doc_roots ships empty. Users register their own via `fs.roots.create`.

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

        -- Typed boolean toggles. Schema enforces enabled IN (0,1); promoted
        -- out of settings so readers don't parse free-form TEXT.
        CREATE TABLE IF NOT EXISTS feature_flags (
            name       TEXT PRIMARY KEY,
            enabled    INTEGER NOT NULL CHECK (enabled IN (0, 1)),
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        INSERT OR IGNORE INTO feature_flags (name, enabled) VALUES
            ('fs.allow_unrestricted',      0),
            ('ui.enabled',                 1),
            ('auth.public_signup_enabled', 0);

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
            created_at      INTEGER NOT NULL,
            code_plain      TEXT
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

        -- host_addressing: this host's multi-channel addresses (display name,
        -- LAN v4/v6, Tailscale, FQDN, …). Keyed by channel kind; rebuilt by
        -- the host_identity refresh job. Mirrors the dial-target snapshot
        -- pod/ping shares with peers.
        CREATE TABLE IF NOT EXISTS host_addressing (
            key         TEXT PRIMARY KEY,
            value       TEXT NOT NULL,
            source      TEXT NOT NULL,
            detected_at INTEGER NOT NULL
        );

        -- pod_peer_addresses: per-peer multi-channel address records, mirrored
        -- in via pod/ping. Augments pod_peers (which holds a single primary
        -- addr) with every kind we've seen.
        CREATE TABLE IF NOT EXISTS pod_peer_addresses (
            peer_id      TEXT NOT NULL,
            kind         TEXT NOT NULL,
            value        TEXT NOT NULL,
            source       TEXT NOT NULL,
            last_seen_at INTEGER NOT NULL,
            PRIMARY KEY (peer_id, kind, value),
            FOREIGN KEY (peer_id) REFERENCES pod_peers(peer_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_pod_peer_addresses_peer
            ON pod_peer_addresses(peer_id);

        -- REST/MCP API bearer tokens. token_hash is sha256(plaintext); the
        -- raw token is returned exactly once from auth.token_create and is
        -- never recoverable from the DB. See project_rest_auth_design.md.
        CREATE TABLE IF NOT EXISTS api_tokens (
            id           TEXT PRIMARY KEY,
            name         TEXT NOT NULL UNIQUE,
            token_hash   TEXT NOT NULL UNIQUE,
            role         TEXT NOT NULL CHECK (role IN ('admin','read')),
            created_at   TEXT NOT NULL,
            last_used_at TEXT,
            expires_at   TEXT,
            -- Issuing user. NULL for tokens minted before user binding existed
            -- (pre-2026-05-29). New tokens record the authenticated operator
            -- so REST bearer-auth produces a CallerIdentity for pod/exec
            -- caller-token minting. See [[project-remote-exec-full-fix]] S4.
            user_id      TEXT,
            -- Opt-in: a read token with can_mutate = 1 may invoke DATA_MUTATION
            -- tools that would otherwise need admin. Never reaches control-plane
            -- admin tools. Default off. See 20260707000000__auth_can_mutate.
            can_mutate   INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens(token_hash);

        -- Web-UI account auth. Per project_rest_auth_v2.md:
        --   * username UNIQUE is case-insensitive — username_lower is the canonical key.
        --   * password_hash is argon2id (encoded form: \"$argon2id$...\").
        --   * sessions slide on every authenticated request (last_used_at, expires_at refresh).
        -- users is ONE shared pool replicated across every paired host (any host
        -- may write; last-write-wins on `updated_at`), so any admin can sign in
        -- on any machine/UI. See project_unified_mesh_state.md (shared policy).
        CREATE TABLE IF NOT EXISTS users (
            id                  TEXT PRIMARY KEY,
            username            TEXT NOT NULL,
            username_lower      TEXT NOT NULL UNIQUE,
            password_hash       TEXT NOT NULL,
            role                TEXT NOT NULL CHECK (role IN ('admin','member')),
            created_at          TEXT NOT NULL,
            password_updated_at TEXT NOT NULL,
            updated_at          TEXT NOT NULL DEFAULT '1970-01-01T00:00:00Z'
        );

        CREATE TABLE IF NOT EXISTS sessions (
            id           TEXT PRIMARY KEY,
            user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            created_at   TEXT NOT NULL,
            last_used_at TEXT NOT NULL,
            expires_at   TEXT NOT NULL,
            revoked_at   TEXT,
            -- Opt-in mirror of api_tokens.can_mutate for browser sessions.
            -- Default off. See 20260707000000__auth_can_mutate.
            can_mutate   INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_user_active
            ON sessions(user_id, expires_at) WHERE revoked_at IS NULL;

        -- Per-peer system snapshot timeseries. source='local' rows are written
        -- by this host's persistence task; source='synced' rows are mirrored
        -- in from the peer's own DB by the sync puller. (peer_id, snapshot_at)
        -- PK makes duplicate sync imports a no-op (INSERT OR IGNORE). Per-peer
        -- retention cap is enforced inside host_status::insert_status.
        CREATE TABLE IF NOT EXISTS host_status (
            peer_id          TEXT    NOT NULL,
            snapshot_at_unix INTEGER NOT NULL,
            payload_json     TEXT    NOT NULL,
            received_at_unix INTEGER NOT NULL,
            source           TEXT    NOT NULL CHECK (source IN ('local','synced')),
            PRIMARY KEY (peer_id, snapshot_at_unix)
        );
        CREATE INDEX IF NOT EXISTS idx_host_status_peer_time
            ON host_status (peer_id, snapshot_at_unix DESC);
        ",
    )?;
    // Toolkit-emitted tables (endpoint_resource! and friends) register
    // their CREATE TABLE statements through inventory. Apply them after
    // the hand-coded schema so any cross-table FKs upstream still resolve.
    schema_fragments::apply_fragments(conn)?;
    Ok(())
}

// ── Key management ───────────────────────────────────────────────────────────

/// Load the DB encryption key from `~/.orca/.db_key`, generating it on first run.
///
/// The key file is the backup unit alongside orca.db — copy both to restore.
/// Never regenerate silently: if the file exists but is unreadable/corrupt, bail
/// so the user knows they need to restore the key rather than destroying their data.
fn load_or_create_key() -> Result<String> {
    // Serialize key bootstrap across every connection in THIS process. On first
    // boot the daemon opens the DB from many tasks at once (migrations + mdns +
    // host_status + replicate); an unguarded check-then-create let each generate
    // a DIFFERENT key with last-write-wins, so orca.db got encrypted with one
    // key while .db_key on disk held another → "database key rejected" on every
    // subsequent open. The lock makes generation happen exactly once per process.
    static KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = KEY_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let key_path = contract::config::state_dir()?.join(".db_key");

    if let Some(key) = read_key_file(&key_path)? {
        return Ok(key);
    }

    // First run: generate key. Use getrandom directly — rand 0.10 reorganized
    // its OS RNG surface and for a one-shot 32-byte crypto key we don't need a
    // full RNG abstraction.
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("OS RNG failure generating db key: {e}"))?;
    let hex: String = bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    });

    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create atomically (O_EXCL) so a racing PROCESS can't clobber our key after
    // we've encrypted the DB with it. If another process won the race, adopt its
    // key instead of overwriting.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&key_path) {
        Ok(mut f) => {
            use std::io::Write as _;
            f.write_all(hex.as_bytes())
                .context("failed to write .db_key")?;
            f.sync_all().ok();
            tracing::info!(
                "generated new DB encryption key at {} — back this up alongside orca.db",
                key_path.display()
            );
            Ok(hex)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_key_file(&key_path)?
            .context(".db_key created by a racing process but is unreadable"),
        Err(e) => Err(e).context("failed to create .db_key"),
    }
}

/// Read + validate the on-disk DB key. `Ok(None)` when the file is absent;
/// `Err` when present but corrupt — never silently regenerate, that would orphan
/// an existing encrypted orca.db.
fn read_key_file(key_path: &std::path::Path) -> Result<Option<String>> {
    match std::fs::read_to_string(key_path) {
        Ok(raw) => {
            let key = raw.trim().to_string();
            anyhow::ensure!(
                key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()),
                "{} is corrupt (expected 64 hex chars) — restore from backup",
                key_path.display()
            );
            Ok(Some(key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).context("failed to read .db_key — restore from backup or run `orca db reset`")
        }
    }
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
            id, session, project, timestamp, role, agent, content, important, tags,
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
            important: row.get(7)?,
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
            important: row.get(7)?,
            tags: row.get(8)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn fs_allow_unrestricted(conn: &Connection) -> bool {
    feature_flags::get(conn, "fs.allow_unrestricted")
        .ok()
        .flatten()
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

    #[test]
    fn ensure_rollback_journal_yields_delete_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rj.db");
        with_thread_db_path(&path, || {
            // Force the fresh db into WAL first so the conversion has real work.
            {
                let conn = open_default().expect("open");
                conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
                    .expect("set wal");
            }
            ensure_rollback_journal().expect("ensure_rollback_journal");
            let conn = open_default().expect("reopen");
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .expect("read journal_mode");
            assert_eq!(mode.to_ascii_lowercase(), "delete");
        });
    }

    #[test]
    fn with_thread_db_path_pins_and_restores_on_return() {
        set_thread_db_path(Some("/orig/path.db"));
        let observed = with_thread_db_path(std::path::Path::new("/scoped/path.db"), || {
            THREAD_DB_PATH.with(|p| p.borrow().clone())
        });
        assert_eq!(observed.as_deref(), Some("/scoped/path.db"));
        assert_eq!(
            THREAD_DB_PATH.with(|p| p.borrow().clone()).as_deref(),
            Some("/orig/path.db"),
            "previous override must be restored after the scope ends"
        );
        set_thread_db_path(None);
    }

    #[test]
    fn with_thread_db_path_restores_on_panic() {
        // Guarantees a panicking test body doesn't leak its override into
        // the next test scheduled on this thread.
        set_thread_db_path(None);
        let result = std::panic::catch_unwind(|| {
            with_thread_db_path(std::path::Path::new("/leak/path.db"), || {
                panic!("boom");
            })
        });
        assert!(result.is_err());
        assert!(
            THREAD_DB_PATH.with(|p| p.borrow().is_none()),
            "override must be cleared after panic unwind"
        );
    }

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
        // Seeded as 0 in the feature_flags baseline.
        assert!(!fs_allow_unrestricted(&conn));
        feature_flags::set(&conn, "fs.allow_unrestricted", true).unwrap();
        assert!(fs_allow_unrestricted(&conn));
    }
}
