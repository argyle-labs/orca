//! Encrypted SQLite database (`orca.db`) — the runtime registry for all dynamic orca config.
//!
//! `open_default()` is the standard entry point. It opens (or creates) `~/.orca/orca.db`,
//! applies the SQLCipher encryption key, runs `apply_schema` to ensure all tables exist,
//! then applies any pending schema migrations via `run_pending_migrations`.
//!
//! Adding a new registry feature means adding a table in `apply_schema`, CRUD helpers at
//! the bottom of this file, and a migration entry in `MIGRATIONS` if the table was added
//! to an already-deployed database.

pub mod startup;

use anyhow::{Context, Result};
use config::{APP_DB_FILE, APP_STATE_DIR};
use rand::RngCore;
use rusqlite::Connection;
use std::path::Path;

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

fn to_json_arr<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn to_json_obj<T: serde::Serialize>(v: &T) -> String {
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

/// Open (or create) the encrypted orca database.
///
/// Key is loaded from the OS keychain on first call; generated and stored if not found.
/// The database file lives at `~/.orca/orca.db` by default.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path).context("failed to open database")?;

    // Load or generate the 32-byte encryption key
    let key_hex = load_or_create_key()?;
    // SQLCipher hex key syntax: x'...' — bypasses PBKDF2 and uses the raw key directly
    conn.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\";"))
        .context("failed to apply SQLCipher key")?;

    // Verify the key works (SQLCipher returns an error on wrong key when first accessing data)
    conn.execute_batch("PRAGMA user_version;")
        .context("database key rejected — key mismatch or corrupted database")?;

    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    apply_schema(&conn)?;
    run_pending_migrations(&conn)?;

    Ok(conn)
}

// Thread-local DB path override — each test thread can set its own isolated DB
// path without racing against other parallel tests. Takes priority over ORCA_DB_PATH.
thread_local! {
    static THREAD_DB_PATH: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Set a per-thread DB path override (for integration tests only).
/// Pass `None` to clear the override and restore default lookup.
pub fn set_thread_db_path(path: Option<&str>) {
    THREAD_DB_PATH.with(|p| *p.borrow_mut() = path.map(|s| s.to_string()));
}

/// Open orca database using the default path (`~/.orca/orca.db`).
///
/// Resolution order:
///   1. Thread-local override set by `set_thread_db_path` (integration tests).
///   2. `ORCA_DB_PATH` env var (legacy / CI override).
///   3. `~/.orca/orca.db` (encrypted, production).
pub fn open_default() -> Result<Connection> {
    let thread_path = THREAD_DB_PATH.with(|p| p.borrow().clone());
    if let Some(path) = thread_path {
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
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    apply_schema(&conn)?;
    run_pending_migrations(&conn)?;
    Ok(conn)
}

// ── Migrations ───────────────────────────────────────────────────────────────

/// Direction to migrate: one step up or one step down.
pub enum MigrateDirection {
    Up,
    Down,
}

struct Migration {
    version: u32,
    description: &'static str,
    up: &'static str,
    /// `None` means the migration cannot be reversed (e.g. SQLite DROP COLUMN unavailable).
    down: Option<&'static str>,
}

/// All schema migrations in version order.
///
/// Rules:
/// - Never modify an existing entry — add new entries at the end.
/// - `up` must be idempotent-safe: use `IF NOT EXISTS`, `IF EXISTS`, or handle
///   `duplicate column` errors in `run_pending_migrations`.
/// - `down` is `None` when rollback is impossible (SQLite < 3.35 has no DROP COLUMN).
static MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "add url column to docker_runtimes",
        up: "ALTER TABLE docker_runtimes ADD COLUMN url TEXT;",
        down: None,
    },
    Migration {
        version: 2,
        description: "rename orca_tool column to orca_tool in mcp_tool_mappings",
        up: "ALTER TABLE mcp_tool_mappings RENAME COLUMN orca_tool TO orca_tool;",
        down: None,
    },
    Migration {
        version: 3,
        description: "add command_map to plugins for universal command routing",
        up: "ALTER TABLE plugins ADD COLUMN command_map TEXT NOT NULL DEFAULT '{}';",
        down: None,
    },
    Migration {
        version: 4,
        description: "add plugin_credentials table — Orca-managed secrets per plugin",
        up: "CREATE TABLE IF NOT EXISTS plugin_credentials (
            plugin_id  TEXT NOT NULL,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            synced_at  TEXT,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (plugin_id, key)
        );",
        down: Some("DROP TABLE IF EXISTS plugin_credentials;"),
    },
    Migration {
        version: 5,
        description: "add mcp_token_env to plugins — env var name carrying Bearer token for HTTP/SSE transport",
        up: "ALTER TABLE plugins ADD COLUMN mcp_token_env TEXT;",
        down: None,
    },
    Migration {
        version: 6,
        description: "add driver column to schema_databases (mysql/postgres/sqlite)",
        up: "ALTER TABLE schema_databases ADD COLUMN driver TEXT NOT NULL DEFAULT 'mysql';",
        down: None,
    },
    Migration {
        version: 7,
        description: "add oauth_tokens table — service access/refresh tokens with expiry",
        up: "CREATE TABLE IF NOT EXISTS oauth_tokens (
            service       TEXT PRIMARY KEY,
            access_token  TEXT NOT NULL,
            refresh_token TEXT,
            expires_at    TEXT,
            updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );",
        down: Some("DROP TABLE IF EXISTS oauth_tokens;"),
    },
    Migration {
        version: 8,
        description: "add mode + nav_links to plugins — plugins declare which mode they belong to and what nav links they contribute",
        up: "ALTER TABLE plugins ADD COLUMN mode TEXT NOT NULL DEFAULT 'orca'; \
             ALTER TABLE plugins ADD COLUMN nav_links TEXT NOT NULL DEFAULT '[]';",
        down: None,
    },
    Migration {
        version: 9,
        description: "add plugin_data table — generic KV store for plugin-owned data in Orca",
        up: "CREATE TABLE IF NOT EXISTS plugin_data (
            plugin_id  TEXT NOT NULL,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (plugin_id, key)
        );",
        down: Some("DROP TABLE IF EXISTS plugin_data;"),
    },
    Migration {
        version: 10,
        description: "add search_tools to plugins — MCP tool names this plugin exposes for unified orca search",
        up: "ALTER TABLE plugins ADD COLUMN search_tools TEXT NOT NULL DEFAULT '[]';",
        down: None,
    },
    Migration {
        version: 11,
        description: "add specs_dir to plugins — filesystem path where plugin-owned spec files live",
        up: "ALTER TABLE plugins ADD COLUMN specs_dir TEXT;",
        down: None,
    },
    Migration {
        version: 12,
        description: "add llm_providers — registered local LLM backends (LM Studio, Ollama, …)",
        up: "CREATE TABLE IF NOT EXISTS llm_providers (
            name       TEXT PRIMARY KEY,
            url        TEXT NOT NULL,
            kind       TEXT NOT NULL DEFAULT 'lmstudio',
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );",
        down: Some("DROP TABLE IF EXISTS llm_providers;"),
    },
    Migration {
        version: 13,
        description: "add plugin_deps — tracks which plugins were auto-installed as dependencies of another",
        up: "CREATE TABLE IF NOT EXISTS plugin_deps (
            parent_id  TEXT NOT NULL,
            dep_id     TEXT NOT NULL,
            PRIMARY KEY (parent_id, dep_id)
        );",
        down: Some("DROP TABLE IF EXISTS plugin_deps;"),
    },
    Migration {
        version: 14,
        description: "add mcp_url to plugins — HTTP/SSE endpoint used instead of stdio when present (deploy mode)",
        up: "ALTER TABLE plugins ADD COLUMN mcp_url TEXT;",
        down: None,
    },
    Migration {
        version: 15,
        description: "add doc_roots table — user-configurable documentation path registry",
        up: "CREATE TABLE IF NOT EXISTS doc_roots (
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
            ('meerkat',  '~/code/meerkat',  'Meerkat');",
        down: Some("DROP TABLE IF EXISTS doc_roots;"),
    },
    Migration {
        version: 16,
        description: "add doc_ignore_patterns table — global list of directory names excluded from all doc roots",
        up: "CREATE TABLE IF NOT EXISTS doc_ignore_patterns (
            pattern    TEXT PRIMARY KEY,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        INSERT OR IGNORE INTO doc_ignore_patterns (pattern) VALUES
            ('.git'), ('node_modules'), ('target'), ('.next'), ('dist'),
            ('build'), ('vendor'), ('.trash'), ('logs'), ('memory'),
            ('plugins'), ('.turbo'), ('coverage'), ('out'), ('.cache');",
        down: Some("DROP TABLE IF EXISTS doc_ignore_patterns;"),
    },
    Migration {
        version: 17,
        description: "add settings table — generic key/value store for user-configurable flags",
        up: "CREATE TABLE IF NOT EXISTS settings (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        INSERT OR IGNORE INTO settings (key, value) VALUES ('fs.allow_unrestricted', 'false');",
        down: Some("DROP TABLE IF EXISTS settings;"),
    },
    Migration {
        version: 18,
        description: "add plugin_types — per-plugin TypedValue type registry declared via orca/types.declare",
        up: "CREATE TABLE IF NOT EXISTS plugin_types (
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
        CREATE INDEX IF NOT EXISTS idx_plugin_types_fq ON plugin_types(fq_type_id);",
        down: Some("DROP TABLE IF EXISTS plugin_types;"),
    },
];

/// Return the currently applied migration version (0 = baseline, no migrations run).
pub fn schema_version(conn: &Connection) -> Result<u32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Total number of migrations defined (applied + pending combined).
pub fn migration_count() -> usize {
    MIGRATIONS.len()
}

/// Run pending up-migrations automatically after `apply_schema`.
///
/// Handles the "duplicate column" case for `ALTER TABLE ADD COLUMN` migrations so
/// existing databases that were mutated by the pre-migration `let _` hack continue
/// to work.
fn run_pending_migrations(conn: &Connection) -> Result<()> {
    migrate(conn, MigrateDirection::Up, usize::MAX)?;
    Ok(())
}

/// Apply or revert migrations.
///
/// - `Up` with `steps = usize::MAX` runs all pending migrations (idiomatic for startup).
/// - `Up` with `steps = 1` applies the next pending migration.
/// - `Down` with `steps = 1` reverts the most recently applied migration.
///
/// Returns the new `user_version` after all steps are applied.
pub fn migrate(conn: &Connection, direction: MigrateDirection, steps: usize) -> Result<u32> {
    let current = schema_version(conn)?;

    match direction {
        MigrateDirection::Up => {
            let pending: Vec<&Migration> = MIGRATIONS
                .iter()
                .filter(|m| m.version > current)
                .take(steps)
                .collect();

            if pending.is_empty() {
                eprintln!("  nothing to migrate (schema at v{current})");
            }

            for m in pending {
                if let Err(e) = conn.execute_batch(m.up) {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("duplicate column") || msg.contains("already exists") {
                        // Column/table already present — idempotent, mark as done.
                    } else {
                        return Err(anyhow::anyhow!("migration v{} failed: {e}", m.version));
                    }
                }
                conn.execute_batch(&format!("PRAGMA user_version = {};", m.version))?;
                eprintln!("  ↑  v{}: {}", m.version, m.description);
            }
        }

        MigrateDirection::Down => {
            let to_rollback: Vec<&Migration> = MIGRATIONS
                .iter()
                .filter(|m| m.version <= current)
                .rev()
                .take(steps)
                .collect();

            if to_rollback.is_empty() {
                eprintln!("  nothing to roll back (schema at v{current})");
            }

            for m in to_rollback {
                match m.down {
                    Some(sql) => {
                        conn.execute_batch(sql)?;
                        eprintln!("  ↓  v{}: {}", m.version, m.description);
                    }
                    None => {
                        eprintln!("  ~  v{}: no down migration — {}", m.version, m.description);
                    }
                }
                let new_version = m.version.saturating_sub(1);
                conn.execute_batch(&format!("PRAGMA user_version = {};", new_version))?;
            }
        }
    }

    schema_version(conn)
}

// ── Schema ───────────────────────────────────────────────────────────────────

fn apply_schema(conn: &Connection) -> Result<()> {
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
            tags        TEXT    -- JSON array e.g. '[\"bug\",\"fix\"]'
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
            orca_tool      TEXT PRIMARY KEY,
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

        CREATE TABLE IF NOT EXISTS plugins (
            id                TEXT PRIMARY KEY,
            manifest_path     TEXT NOT NULL,
            tier              TEXT NOT NULL DEFAULT 'personal',
            mcp_command       TEXT,
            mcp_args          TEXT NOT NULL DEFAULT '[]',
            mcp_env           TEXT NOT NULL DEFAULT '{}',
            context_injection TEXT NOT NULL DEFAULT 'minimal',
            enabled           INTEGER NOT NULL DEFAULT 1,
            created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS plugin_data (
            plugin_id  TEXT NOT NULL,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (plugin_id, key)
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

    // First run: generate key, write with restricted permissions
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
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

// ── MCP server registry ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpServerRow {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub enabled: bool,
}

pub fn list_mcp_servers(conn: &Connection) -> Result<Vec<McpServerRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, command, args, env, enabled FROM mcp_servers WHERE enabled = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i32>(4)?,
        ))
    })?;
    let mut result = Vec::new();
    for r in rows {
        let (name, command, args_json, env_json, enabled) = r?;
        let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
        let env: std::collections::HashMap<String, String> =
            serde_json::from_str(&env_json).unwrap_or_default();
        result.push(McpServerRow {
            name,
            command,
            args,
            env,
            enabled: enabled != 0,
        });
    }
    Ok(result)
}

pub fn upsert_mcp_server(conn: &Connection, server: &McpServerRow) -> Result<()> {
    let args_json = to_json_arr(&server.args);
    let env_json = to_json_obj(&server.env);
    conn.execute(
        "INSERT INTO mcp_servers (name, command, args, env, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
             command = excluded.command,
             args    = excluded.args,
             env     = excluded.env,
             enabled = excluded.enabled",
        rusqlite::params![
            server.name,
            server.command,
            args_json,
            env_json,
            server.enabled as i32
        ],
    )?;
    Ok(())
}

pub fn remove_mcp_server(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM mcp_servers WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

// ── Schema database registry ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SchemaDbRow {
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    pub database: String,
    pub container: Option<String>,
    pub domains_file: Option<String>,
    pub enabled: bool,
}

pub fn list_schema_databases(conn: &Connection) -> Result<Vec<SchemaDbRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, host, port, user, password, database, container, domains_file, enabled,
                COALESCE(driver, 'mysql')
         FROM schema_databases WHERE enabled = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SchemaDbRow {
            name: row.get(0)?,
            host: row.get(1)?,
            port: row.get::<_, Option<i64>>(2)?.map(|p| p as u16),
            user: row.get(3)?,
            password: row.get(4)?,
            database: row.get(5)?,
            container: row.get(6)?,
            domains_file: row.get(7)?,
            enabled: row.get::<_, i32>(8)? != 0,
            driver: row.get(9)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn upsert_schema_database(conn: &Connection, db: &SchemaDbRow) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_databases (name, host, port, user, password, database, container, domains_file, enabled, driver)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(name) DO UPDATE SET
             host         = excluded.host,
             port         = excluded.port,
             user         = excluded.user,
             password     = excluded.password,
             database     = excluded.database,
             container    = excluded.container,
             domains_file = excluded.domains_file,
             enabled      = excluded.enabled,
             driver       = excluded.driver",
        rusqlite::params![
            db.name,
            db.host,
            db.port.map(|p| p as i64),
            db.user,
            db.password,
            db.database,
            db.container,
            db.domains_file,
            db.enabled as i32,
            db.driver,
        ],
    )?;
    Ok(())
}

pub fn remove_schema_database(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM schema_databases WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

// ── Docker runtime registry ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DockerRuntimeRow {
    pub name: String,
    /// Path to the unix socket (e.g. `~/.colima/default/docker.sock`)
    pub socket_path: Option<String>,
    /// Full DOCKER_HOST URL for TCP remotes (e.g. `tcp://remote:2376`)
    pub host: Option<String>,
    /// HTTP URL for web-based orchestrators (Dockge, Portainer, etc.)
    pub url: Option<String>,
    pub enabled: bool,
}

impl DockerRuntimeRow {
    /// Returns the DOCKER_HOST value to inject into subprocess environments.
    /// Only applies to socket/tcp runtimes — web-based runtimes (url only) return None.
    pub fn docker_host(&self) -> Option<String> {
        if let Some(sock) = &self.socket_path {
            let expanded = expand_tilde(sock);
            Some(format!("unix://{expanded}"))
        } else {
            self.host.clone()
        }
    }
}

pub fn list_docker_runtimes(conn: &Connection) -> Result<Vec<DockerRuntimeRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, socket_path, host, url, enabled FROM docker_runtimes ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(DockerRuntimeRow {
            name: row.get(0)?,
            socket_path: row.get(1)?,
            host: row.get(2)?,
            url: row.get(3)?,
            enabled: row.get::<_, i32>(4)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Returns the first enabled socket/tcp runtime's DOCKER_HOST value for subprocess injection.
/// Web-only runtimes (url, no socket_path/host) are skipped.
pub fn active_docker_host(conn: &Connection) -> Option<String> {
    let mut stmt = conn
        .prepare(
            "SELECT socket_path, host FROM docker_runtimes
             WHERE enabled = 1 AND (socket_path IS NOT NULL OR host IS NOT NULL)
             ORDER BY name LIMIT 1",
        )
        .ok()?;
    let (socket_path, host) = stmt
        .query_row([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .ok()?;
    if let Some(sock) = socket_path {
        Some(format!("unix://{}", expand_tilde(&sock)))
    } else {
        host
    }
}

pub fn upsert_docker_runtime(conn: &Connection, rt: &DockerRuntimeRow) -> Result<()> {
    conn.execute(
        "INSERT INTO docker_runtimes (name, socket_path, host, url, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
             socket_path = excluded.socket_path,
             host        = excluded.host,
             url         = excluded.url,
             enabled     = excluded.enabled",
        rusqlite::params![rt.name, rt.socket_path, rt.host, rt.url, rt.enabled as i32],
    )?;
    Ok(())
}

pub fn remove_docker_runtime(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM docker_runtimes WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

// ── OpenAPI spec registry ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OpenApiSpecRow {
    pub name: String,
    pub url: Option<String>,
    pub source_mcp: Option<String>,
    pub spec_json: Option<String>,
    pub cached_at: Option<String>,
    pub enabled: bool,
}

pub fn list_openapi_specs(conn: &Connection) -> Result<Vec<OpenApiSpecRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, url, source_mcp, spec_json, cached_at, enabled
         FROM openapi_specs ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(OpenApiSpecRow {
            name: row.get(0)?,
            url: row.get(1)?,
            source_mcp: row.get(2)?,
            spec_json: row.get(3)?,
            cached_at: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get_openapi_spec(conn: &Connection, name: &str) -> Result<Option<OpenApiSpecRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, url, source_mcp, spec_json, cached_at, enabled
         FROM openapi_specs WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![name], |row| {
        Ok(OpenApiSpecRow {
            name: row.get(0)?,
            url: row.get(1)?,
            source_mcp: row.get(2)?,
            spec_json: row.get(3)?,
            cached_at: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
        })
    })?;
    Ok(rows.next().transpose()?)
}

pub fn upsert_openapi_spec(conn: &Connection, spec: &OpenApiSpecRow) -> Result<()> {
    conn.execute(
        "INSERT INTO openapi_specs (name, url, source_mcp, spec_json, cached_at, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name) DO UPDATE SET
             url        = excluded.url,
             source_mcp = excluded.source_mcp,
             spec_json  = excluded.spec_json,
             cached_at  = excluded.cached_at,
             enabled    = excluded.enabled",
        rusqlite::params![
            spec.name,
            spec.url,
            spec.source_mcp,
            spec.spec_json,
            spec.cached_at,
            spec.enabled as i32,
        ],
    )?;
    Ok(())
}

pub fn remove_openapi_spec(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM openapi_specs WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

// ── MCP tool mapping registry ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpToolMappingRow {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
    pub match_type: String,
    pub confidence: Option<f64>,
    pub enabled: bool,
}

pub fn list_mcp_tool_mappings(conn: &Connection, mcp_name: &str) -> Result<Vec<McpToolMappingRow>> {
    let mut stmt = conn.prepare(
        "SELECT orca_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE mcp_name = ?1 ORDER BY orca_tool",
    )?;
    let rows = stmt.query_map(rusqlite::params![mcp_name], |row| {
        Ok(McpToolMappingRow {
            orca_tool: row.get(0)?,
            mcp_name: row.get(1)?,
            external_tool: row.get(2)?,
            match_type: row.get(3)?,
            confidence: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn all_mcp_tool_mappings(conn: &Connection) -> Result<Vec<McpToolMappingRow>> {
    let mut stmt = conn.prepare(
        "SELECT orca_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE enabled = 1 ORDER BY orca_tool",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(McpToolMappingRow {
            orca_tool: row.get(0)?,
            mcp_name: row.get(1)?,
            external_tool: row.get(2)?,
            match_type: row.get(3)?,
            confidence: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn lookup_mcp_mapping(conn: &Connection, orca_tool: &str) -> Result<Option<McpToolMappingRow>> {
    let result = conn.query_row(
        "SELECT orca_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE orca_tool = ?1 AND enabled = 1",
        rusqlite::params![orca_tool],
        |row| {
            Ok(McpToolMappingRow {
                orca_tool: row.get(0)?,
                mcp_name: row.get(1)?,
                external_tool: row.get(2)?,
                match_type: row.get(3)?,
                confidence: row.get(4)?,
                enabled: row.get::<_, i32>(5)? != 0,
            })
        },
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_mcp_tool_mapping(conn: &Connection, row: &McpToolMappingRow) -> Result<()> {
    conn.execute(
        "INSERT INTO mcp_tool_mappings (orca_tool, mcp_name, external_tool, match_type, confidence, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(orca_tool) DO UPDATE SET
             mcp_name      = excluded.mcp_name,
             external_tool = excluded.external_tool,
             match_type    = excluded.match_type,
             confidence    = excluded.confidence,
             enabled       = excluded.enabled",
        rusqlite::params![
            row.orca_tool, row.mcp_name, row.external_tool,
            row.match_type, row.confidence, row.enabled as i32
        ],
    )?;
    Ok(())
}

pub fn remove_mcp_tool_mapping(conn: &Connection, orca_tool: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM mcp_tool_mappings WHERE orca_tool = ?1",
        rusqlite::params![orca_tool],
    )?;
    Ok(n > 0)
}

pub fn set_mcp_tool_mapping_enabled(
    conn: &Connection,
    orca_tool: &str,
    enabled: bool,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE mcp_tool_mappings SET enabled = ?1 WHERE orca_tool = ?2",
        rusqlite::params![enabled as i32, orca_tool],
    )?;
    Ok(n > 0)
}

// ── Plugin registry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub id: String,
    pub manifest_path: String,
    pub tier: String,
    /// UI mode this plugin belongs to: "orca" (default) or any custom mode string (e.g. "rebuy").
    /// Plugins with the same mode group together in the sidebar.
    pub mode: String,
    pub mcp_command: Option<String>,
    pub mcp_args: Vec<String>,
    pub mcp_env: std::collections::HashMap<String, String>,
    /// Env var name whose value is the Bearer token for HTTP/SSE transport.
    pub mcp_token_env: Option<String>,
    /// HTTP/SSE endpoints for this plugin's MCP server, tried in priority order.
    /// Allows fallback across public domain → LAN → tailscale addresses.
    /// When non-empty, used instead of spawning a stdio subprocess (deploy mode).
    pub mcp_urls: Vec<String>,
    pub context_injection: String,
    pub enabled: bool,
    /// Maps universal command name → plugin's internal MCP tool name.
    pub command_map: std::collections::HashMap<String, String>,
    /// Sidebar nav links this plugin contributes when its mode is active.
    /// JSON array of {href, label} objects, optionally with {section} for grouping.
    pub nav_links: Vec<serde_json::Value>,
    /// MCP tools this plugin exposes for orca's unified search (Cmd+K).
    pub search_tools: Vec<PluginSearchTool>,
    /// Filesystem path to the directory containing this plugin's spec files.
    /// Files here are served by orca's spec system with the plugin's id as namespace.
    pub specs_dir: Option<String>,
}

const PLUGIN_COLS: &str =
    "id, manifest_path, tier, COALESCE(mode,'orca'), mcp_command, mcp_args, mcp_env,
     context_injection, enabled, command_map, mcp_token_env, COALESCE(nav_links,'[]'),
     COALESCE(search_tools,'[]'), specs_dir, mcp_url";

#[allow(clippy::too_many_arguments)]
fn parse_plugin_row(
    id: String,
    manifest_path: String,
    tier: String,
    mode: String,
    mcp_command: Option<String>,
    args_json: String,
    env_json: String,
    context_injection: String,
    enabled: i32,
    map_json: String,
    mcp_token_env: Option<String>,
    nav_links_json: String,
    search_tools_json: String,
    specs_dir: Option<String>,
    mcp_url_raw: Option<String>,
) -> PluginRow {
    // mcp_url column stores either a JSON array ["url1","url2"] or a plain URL string.
    let mcp_urls = match mcp_url_raw.as_deref() {
        None | Some("") => vec![],
        Some(s) => serde_json::from_str::<Vec<String>>(s).unwrap_or_else(|_| vec![s.to_string()]),
    };
    PluginRow {
        id,
        manifest_path,
        tier,
        mode,
        mcp_command,
        mcp_args: serde_json::from_str(&args_json).unwrap_or_default(),
        mcp_env: serde_json::from_str(&env_json).unwrap_or_default(),
        mcp_token_env,
        mcp_urls,
        context_injection,
        enabled: enabled != 0,
        command_map: serde_json::from_str(&map_json).unwrap_or_default(),
        nav_links: serde_json::from_str(&nav_links_json).unwrap_or_default(),
        search_tools: serde_json::from_str(&search_tools_json).unwrap_or_default(),
        specs_dir,
    }
}

pub fn list_plugins(conn: &Connection) -> Result<Vec<PluginRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {PLUGIN_COLS} FROM plugins ORDER BY id"))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i32>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, String>(12)?,
            row.get::<_, Option<String>>(13)?,
            row.get::<_, Option<String>>(14)?,
        ))
    })?;
    let mut result = Vec::new();
    for r in rows {
        let (
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        ) = r?;
        result.push(parse_plugin_row(
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        ));
    }
    Ok(result)
}

pub fn get_plugin(conn: &Connection, id: &str) -> Result<Option<PluginRow>> {
    let result = conn.query_row(
        &format!("SELECT {PLUGIN_COLS} FROM plugins WHERE id = ?1"),
        rusqlite::params![id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i32>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
            ))
        },
    );
    match result {
        Ok((
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        )) => Ok(Some(parse_plugin_row(
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        ))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_plugin(conn: &Connection, plugin: &PluginRow) -> Result<()> {
    let args_json = to_json_arr(&plugin.mcp_args);
    let env_json = to_json_obj(&plugin.mcp_env);
    let map_json = to_json_obj(&plugin.command_map);
    let nav_json = to_json_arr(&plugin.nav_links);
    let search_tools_json = to_json_arr(&plugin.search_tools);
    let mcp_url_json: Option<String> = if plugin.mcp_urls.is_empty() {
        None
    } else {
        Some(to_json_arr(&plugin.mcp_urls))
    };
    conn.execute(
        "INSERT INTO plugins (id, manifest_path, tier, mode, mcp_command, mcp_args, mcp_env, context_injection, enabled, command_map, mcp_token_env, nav_links, search_tools, specs_dir, mcp_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
             manifest_path     = excluded.manifest_path,
             tier              = excluded.tier,
             mode              = excluded.mode,
             mcp_command       = excluded.mcp_command,
             mcp_args          = excluded.mcp_args,
             mcp_env           = excluded.mcp_env,
             context_injection = excluded.context_injection,
             enabled           = excluded.enabled,
             command_map       = excluded.command_map,
             mcp_token_env     = excluded.mcp_token_env,
             nav_links         = excluded.nav_links,
             search_tools      = excluded.search_tools,
             specs_dir         = excluded.specs_dir,
             mcp_url           = excluded.mcp_url",
        rusqlite::params![
            plugin.id, plugin.manifest_path, plugin.tier, plugin.mode,
            plugin.mcp_command, args_json, env_json, plugin.context_injection,
            plugin.enabled as i32, map_json, plugin.mcp_token_env, nav_json, search_tools_json,
            plugin.specs_dir, mcp_url_json,
        ],
    )?;
    Ok(())
}

pub fn remove_plugin(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM plugins WHERE id = ?1", rusqlite::params![id])?;
    Ok(n > 0)
}

/// Record that `dep_id` was installed as a dependency of `parent_id`.
pub fn add_plugin_dep(conn: &Connection, parent_id: &str, dep_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO plugin_deps (parent_id, dep_id) VALUES (?1, ?2)",
        rusqlite::params![parent_id, dep_id],
    )?;
    Ok(())
}

/// Return all dep_ids that were pulled in by `parent_id`.
pub fn list_plugin_deps(conn: &Connection, parent_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT dep_id FROM plugin_deps WHERE parent_id = ?1")?;
    let rows = stmt.query_map(rusqlite::params![parent_id], |r| r.get(0))?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

/// Remove all dep records for `parent_id` (called when parent is removed).
pub fn remove_plugin_deps(conn: &Connection, parent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM plugin_deps WHERE parent_id = ?1",
        rusqlite::params![parent_id],
    )?;
    Ok(())
}

/// Return true if `dep_id` is depended on by any other plugin.
pub fn plugin_has_parent(conn: &Connection, dep_id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM plugin_deps WHERE dep_id = ?1",
        rusqlite::params![dep_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn set_plugin_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<bool> {
    let n = conn.execute(
        "UPDATE plugins SET enabled = ?1 WHERE id = ?2",
        rusqlite::params![enabled as i32, id],
    )?;
    Ok(n > 0)
}

// ── Plugin credentials ────────────────────────────────────────────────────────
// Orca is the single source of truth for plugin credentials.
// Values are stored encrypted at rest by SQLCipher.
// Synced to each plugin's local encrypted store via the HTTP /creds API.

#[derive(Debug, Clone)]
pub struct PluginCredentialRow {
    pub plugin_id: String,
    pub key: String,
    pub value: String,
    pub synced_at: Option<String>,
    pub updated_at: String,
}

/// Store or update a credential for a plugin.
pub fn set_plugin_credential(
    conn: &Connection,
    plugin_id: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO plugin_credentials (plugin_id, key, value, synced_at, updated_at)
         VALUES (?1, ?2, ?3, NULL, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(plugin_id, key) DO UPDATE SET
             value      = excluded.value,
             synced_at  = NULL,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![plugin_id, key, value],
    )?;
    Ok(())
}

/// List all credentials for a plugin. Returns key names and metadata; value is included
/// for sync purposes — never surface values in CLI output.
pub fn list_plugin_credentials(
    conn: &Connection,
    plugin_id: &str,
) -> Result<Vec<PluginCredentialRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, key, value, synced_at, updated_at
         FROM plugin_credentials WHERE plugin_id = ?1 ORDER BY key",
    )?;
    let rows = stmt.query_map(rusqlite::params![plugin_id], |row| {
        Ok(PluginCredentialRow {
            plugin_id: row.get(0)?,
            key: row.get(1)?,
            value: row.get(2)?,
            synced_at: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Delete a single credential for a plugin.
pub fn delete_plugin_credential(conn: &Connection, plugin_id: &str, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM plugin_credentials WHERE plugin_id = ?1 AND key = ?2",
        rusqlite::params![plugin_id, key],
    )?;
    Ok(n > 0)
}

/// Mark all credentials for a plugin as synced (called after a successful push).
pub fn mark_plugin_credentials_synced(conn: &Connection, plugin_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE plugin_credentials SET synced_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE plugin_id = ?1",
        rusqlite::params![plugin_id],
    )?;
    Ok(())
}

// ── OAuth token storage ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OAuthTokenRow {
    pub service: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
}

pub fn upsert_oauth_token(conn: &Connection, row: &OAuthTokenRow) -> Result<()> {
    conn.execute(
        "INSERT INTO oauth_tokens (service, access_token, refresh_token, expires_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(service) DO UPDATE SET
             access_token  = excluded.access_token,
             refresh_token = excluded.refresh_token,
             expires_at    = excluded.expires_at,
             updated_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![
            row.service,
            row.access_token,
            row.refresh_token,
            row.expires_at
        ],
    )?;
    Ok(())
}

pub fn get_oauth_token(conn: &Connection, service: &str) -> Result<Option<OAuthTokenRow>> {
    let mut stmt = conn.prepare(
        "SELECT service, access_token, refresh_token, expires_at
         FROM oauth_tokens WHERE service = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![service], |row| {
        Ok(OAuthTokenRow {
            service: row.get(0)?,
            access_token: row.get(1)?,
            refresh_token: row.get(2)?,
            expires_at: row.get(3)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

pub fn delete_oauth_token(conn: &Connection, service: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM oauth_tokens WHERE service = ?1",
        rusqlite::params![service],
    )?;
    Ok(n > 0)
}

// ── Plugin data store ─────────────────────────────────────────────────────────
// Generic encrypted KV store scoped per plugin. Plugins use this to persist
// their own state in Orca's database instead of managing their own files.

#[derive(Debug, Clone)]
pub struct PluginDataRow {
    pub plugin_id: String,
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

pub fn get_plugin_data(
    conn: &Connection,
    plugin_id: &str,
    key: &str,
) -> Result<Option<PluginDataRow>> {
    let result = conn.query_row(
        "SELECT plugin_id, key, value, updated_at FROM plugin_data WHERE plugin_id = ?1 AND key = ?2",
        rusqlite::params![plugin_id, key],
        |row| Ok(PluginDataRow {
            plugin_id: row.get(0)?,
            key:       row.get(1)?,
            value:     row.get(2)?,
            updated_at: row.get(3)?,
        }),
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn set_plugin_data(conn: &Connection, plugin_id: &str, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO plugin_data (plugin_id, key, value, updated_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(plugin_id, key) DO UPDATE SET
             value      = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![plugin_id, key, value],
    )?;
    Ok(())
}

pub fn list_plugin_data(conn: &Connection, plugin_id: &str) -> Result<Vec<PluginDataRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, key, value, updated_at FROM plugin_data WHERE plugin_id = ?1 ORDER BY key",
    )?;
    let rows = stmt.query_map(rusqlite::params![plugin_id], |row| {
        Ok(PluginDataRow {
            plugin_id: row.get(0)?,
            key: row.get(1)?,
            value: row.get(2)?,
            updated_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn delete_plugin_data(conn: &Connection, plugin_id: &str, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM plugin_data WHERE plugin_id = ?1 AND key = ?2",
        rusqlite::params![plugin_id, key],
    )?;
    Ok(n > 0)
}

// ── Settings (generic key/value) ──────────────────────────────────────────────

pub fn settings_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        rusqlite::params![key],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn settings_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

pub fn settings_delete(conn: &Connection, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM settings WHERE key = ?1",
        rusqlite::params![key],
    )?;
    Ok(n > 0)
}

// ── Secrets (settings rows under the `secrets.` prefix) ──────────────────────
//
// These live in the SQLCipher-encrypted `settings` table, so values are at-rest
// encrypted by the same key the rest of the orca DB uses. Read/write through
// these helpers so the prefix stays consistent and we can later add a separate
// table or audit log without touching call sites.

const SECRET_PREFIX: &str = "secrets.";

pub fn secret_get(conn: &Connection, name: &str) -> Result<Option<String>> {
    settings_get(conn, &format!("{SECRET_PREFIX}{name}"))
}

pub fn secret_set(conn: &Connection, name: &str, value: &str) -> Result<()> {
    settings_set(conn, &format!("{SECRET_PREFIX}{name}"), value)
}

pub fn secret_delete(conn: &Connection, name: &str) -> Result<bool> {
    settings_delete(conn, &format!("{SECRET_PREFIX}{name}"))
}

/// Mask an API key for display: first 8 chars + ellipsis + last 4. Short keys (≤12) are fully masked.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 12 {
        let prefix: String = chars[..8].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}…{suffix}")
    } else {
        "****".to_string()
    }
}

/// Returns true if the key looks like an Anthropic key (starts with `sk-ant-`).
pub fn looks_like_anthropic_key(key: &str) -> bool {
    key.starts_with("sk-ant-")
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn mask_key_long_key_shows_first_and_last() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let masked = mask_key(key);
        assert!(masked.starts_with("sk-ant-a"), "prefix wrong: {masked}");
        assert!(masked.ends_with("wxyz"), "suffix wrong: {masked}");
        assert!(masked.contains('…'), "no ellipsis: {masked}");
    }

    #[test]
    fn mask_key_short_returns_stars() {
        assert_eq!(mask_key("short"), "****");
    }

    #[test]
    fn mask_key_exactly_12_returns_stars() {
        assert_eq!(mask_key("abcdefghijkl"), "****");
    }

    #[test]
    fn mask_key_13_chars_masks() {
        let key = "abcdefghijklm";
        let masked = mask_key(key);
        assert!(masked.starts_with("abcdefgh"), "got: {masked}");
        assert!(masked.ends_with("jklm"), "got: {masked}");
    }

    #[test]
    fn mask_key_empty_returns_stars() {
        assert_eq!(mask_key(""), "****");
    }

    #[test]
    fn looks_like_anthropic_accepts_real_format() {
        assert!(looks_like_anthropic_key("sk-ant-api03-xyz"));
        assert!(!looks_like_anthropic_key("sk-1234"));
        assert!(!looks_like_anthropic_key(""));
    }
}

pub fn settings_list_prefix(conn: &Connection, prefix: &str) -> Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT key, value FROM settings WHERE key LIKE ?1 ORDER BY key")?;
    let pattern = format!("{prefix}%");
    let rows = stmt.query_map(rusqlite::params![pattern], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// ── LLM providers ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LlmProvider {
    pub name: String,
    pub url: String,
    pub kind: String,
    pub enabled: bool,
    pub created_at: String,
}

pub fn list_llm_providers(conn: &Connection) -> Result<Vec<LlmProvider>> {
    let mut stmt = conn.prepare(
        "SELECT name, url, kind, enabled, created_at FROM llm_providers ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(LlmProvider {
            name: row.get(0)?,
            url: row.get(1)?,
            kind: row.get(2)?,
            enabled: row.get::<_, i64>(3)? != 0,
            created_at: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn upsert_llm_provider(conn: &Connection, name: &str, url: &str, kind: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_providers (name, url, kind) VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, kind = excluded.kind, enabled = 1",
        rusqlite::params![name, url, kind],
    )?;
    Ok(())
}

pub fn set_llm_provider_enabled(conn: &Connection, name: &str, enabled: bool) -> Result<bool> {
    let n = conn.execute(
        "UPDATE llm_providers SET enabled = ?2 WHERE name = ?1",
        rusqlite::params![name, enabled as i64],
    )?;
    Ok(n > 0)
}

pub fn remove_llm_provider(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM llm_providers WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

// ── Doc root registry ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DocRootRow {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    pub enabled: bool,
}

pub fn list_doc_roots(conn: &Connection) -> Result<Vec<DocRootRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, path, description, enabled FROM doc_roots WHERE enabled = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(DocRootRow {
            name: row.get(0)?,
            path: row.get(1)?,
            description: row.get(2)?,
            enabled: row.get::<_, i32>(3)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn upsert_doc_root(conn: &Connection, root: &DocRootRow) -> Result<()> {
    conn.execute(
        "INSERT INTO doc_roots (name, path, description, enabled)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             path        = excluded.path,
             description = excluded.description,
             enabled     = excluded.enabled",
        rusqlite::params![root.name, root.path, root.description, root.enabled as i32],
    )?;
    Ok(())
}

pub fn remove_doc_root(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM doc_roots WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

// ── Doc ignore patterns ───────────────────────────────────────────────────────

pub fn list_doc_ignore_patterns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT pattern FROM doc_ignore_patterns ORDER BY pattern")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn add_doc_ignore_pattern(conn: &Connection, pattern: &str) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO doc_ignore_patterns (pattern) VALUES (?1)",
        rusqlite::params![pattern],
    )?;
    Ok(n > 0)
}

pub fn remove_doc_ignore_pattern(conn: &Connection, pattern: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM doc_ignore_patterns WHERE pattern = ?1",
        rusqlite::params![pattern],
    )?;
    Ok(n > 0)
}

// ── Settings ──────────────────────────────────────────────────────────────────

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    let val = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        )
        .ok();
    Ok(val)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

pub fn list_settings(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// ── plugin_types CRUD ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginTypeRow {
    pub plugin_id: String,
    pub type_name: String,
    pub fq_type_id: String,
    pub schema_version: String,
    /// Raw JSON Schema text, exactly as the plugin submitted it.
    pub schema_json: String,
    /// "general" | "sensitive"
    pub sensitivity: String,
    pub declared_at: String,
}

/// Upsert a plugin-declared TypedValue type. The fully-qualified id is
/// computed as `<plugin_id>.<type_name>` and is unique across all plugins.
pub fn upsert_plugin_type(
    conn: &Connection,
    plugin_id: &str,
    type_name: &str,
    schema_version: &str,
    schema_json: &str,
    sensitivity: &str,
) -> Result<()> {
    if !matches!(sensitivity, "general" | "sensitive") {
        anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
    }
    let fq = format!("{plugin_id}.{type_name}");
    conn.execute(
        "INSERT INTO plugin_types
            (plugin_id, type_name, fq_type_id, schema_version, schema_json, sensitivity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(plugin_id, type_name) DO UPDATE SET
            schema_version = excluded.schema_version,
            schema_json    = excluded.schema_json,
            sensitivity    = excluded.sensitivity,
            declared_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![
            plugin_id,
            type_name,
            fq,
            schema_version,
            schema_json,
            sensitivity
        ],
    )?;
    Ok(())
}

/// List all types declared by a single plugin.
pub fn list_plugin_types(conn: &Connection, plugin_id: &str) -> Result<Vec<PluginTypeRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, type_name, fq_type_id, schema_version, schema_json, sensitivity, declared_at
         FROM plugin_types WHERE plugin_id = ?1 ORDER BY type_name",
    )?;
    let rows = stmt.query_map([plugin_id], |r| {
        Ok(PluginTypeRow {
            plugin_id: r.get(0)?,
            type_name: r.get(1)?,
            fq_type_id: r.get(2)?,
            schema_version: r.get(3)?,
            schema_json: r.get(4)?,
            sensitivity: r.get(5)?,
            declared_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up a single type by its fully-qualified id (`<plugin_id>.<type_name>`).
pub fn get_plugin_type(conn: &Connection, fq_type_id: &str) -> Result<Option<PluginTypeRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, type_name, fq_type_id, schema_version, schema_json, sensitivity, declared_at
         FROM plugin_types WHERE fq_type_id = ?1",
    )?;
    let row = stmt
        .query_row([fq_type_id], |r| {
            Ok(PluginTypeRow {
                plugin_id: r.get(0)?,
                type_name: r.get(1)?,
                fq_type_id: r.get(2)?,
                schema_version: r.get(3)?,
                schema_json: r.get(4)?,
                sensitivity: r.get(5)?,
                declared_at: r.get(6)?,
            })
        })
        .map(Some)
        .or_else(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
    Ok(row)
}

pub fn fs_allow_unrestricted(conn: &Connection) -> bool {
    get_setting(conn, "fs.allow_unrestricted")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false)
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// Open an unencrypted in-memory database with full schema + migrations applied.
    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        conn.execute_batch("PRAGMA journal_mode = WAL;").ok();
        conn.execute_batch("PRAGMA foreign_keys = ON;").ok();
        apply_schema(&conn).expect("apply_schema");
        run_pending_migrations(&conn).expect("migrations");
        conn
    }

    // ── Migrations ────────────────────────────────────────────────────────────

    #[test]
    fn migrations_run_to_latest() {
        let conn = test_conn();
        let v = schema_version(&conn).unwrap();
        assert_eq!(
            v as usize,
            MIGRATIONS.len(),
            "schema version should match migration count"
        );
    }

    #[test]
    fn migration_count_is_nonzero() {
        assert!(migration_count() > 0);
    }

    #[test]
    fn migrate_up_idempotent_already_at_latest() {
        let conn = test_conn();
        let v_before = schema_version(&conn).unwrap();
        migrate(&conn, MigrateDirection::Up, usize::MAX).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), v_before);
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

    // ── MCP servers ───────────────────────────────────────────────────────────

    #[test]
    fn mcp_server_crud() {
        let conn = test_conn();
        assert!(list_mcp_servers(&conn).unwrap().is_empty());

        let server = McpServerRow {
            name: "test-mcp".into(),
            command: "/usr/bin/node".into(),
            args: vec!["server.js".into()],
            env: [("PORT".into(), "3000".into())].into(),
            enabled: true,
        };
        upsert_mcp_server(&conn, &server).unwrap();

        let list = list_mcp_servers(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-mcp");
        assert_eq!(list[0].args, vec!["server.js"]);
        assert_eq!(list[0].env.get("PORT").map(|s| s.as_str()), Some("3000"));

        assert!(remove_mcp_server(&conn, "test-mcp").unwrap());
        assert!(list_mcp_servers(&conn).unwrap().is_empty());
        assert!(!remove_mcp_server(&conn, "test-mcp").unwrap());
    }

    #[test]
    fn mcp_server_upsert_updates_existing() {
        let conn = test_conn();
        let s = McpServerRow {
            name: "s".into(),
            command: "cmd1".into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
        };
        upsert_mcp_server(&conn, &s).unwrap();
        let s2 = McpServerRow {
            name: "s".into(),
            command: "cmd2".into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
        };
        upsert_mcp_server(&conn, &s2).unwrap();
        let list = list_mcp_servers(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].command, "cmd2");
    }

    // ── MCP tool mappings ─────────────────────────────────────────────────────

    #[test]
    fn mcp_tool_mapping_crud() {
        let conn = test_conn();
        // Need a parent MCP server (FK constraint)
        let server = McpServerRow {
            name: "mcp".into(),
            command: "cmd".into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
        };
        upsert_mcp_server(&conn, &server).unwrap();

        let mapping = McpToolMappingRow {
            orca_tool: "read_file".into(),
            mcp_name: "mcp".into(),
            external_tool: "fs_read".into(),
            match_type: "explicit".into(),
            confidence: Some(0.99),
            enabled: true,
        };
        upsert_mcp_tool_mapping(&conn, &mapping).unwrap();

        let found = lookup_mcp_mapping(&conn, "read_file").unwrap().unwrap();
        assert_eq!(found.external_tool, "fs_read");
        assert!((found.confidence.unwrap() - 0.99).abs() < 1e-9);

        let all = all_mcp_tool_mappings(&conn).unwrap();
        assert_eq!(all.len(), 1);

        let by_server = list_mcp_tool_mappings(&conn, "mcp").unwrap();
        assert_eq!(by_server.len(), 1);

        assert!(set_mcp_tool_mapping_enabled(&conn, "read_file", false).unwrap());
        assert!(
            lookup_mcp_mapping(&conn, "read_file").unwrap().is_none(),
            "disabled should not appear"
        );

        assert!(remove_mcp_tool_mapping(&conn, "read_file").unwrap());
        assert!(!remove_mcp_tool_mapping(&conn, "read_file").unwrap());
    }

    // ── Schema databases ──────────────────────────────────────────────────────

    #[test]
    fn schema_database_crud() {
        let conn = test_conn();
        assert!(list_schema_databases(&conn).unwrap().is_empty());

        let db = SchemaDbRow {
            name: "mydb".into(),
            driver: "postgres".into(),
            host: Some("localhost".into()),
            port: Some(5432),
            user: "admin".into(),
            password: "secret".into(),
            database: "app".into(),
            container: None,
            domains_file: None,
            enabled: true,
        };
        upsert_schema_database(&conn, &db).unwrap();

        let list = list_schema_databases(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "mydb");
        assert_eq!(list[0].port, Some(5432));
        assert_eq!(list[0].driver, "postgres");

        assert!(remove_schema_database(&conn, "mydb").unwrap());
        assert!(list_schema_databases(&conn).unwrap().is_empty());
    }

    // ── Docker runtimes ───────────────────────────────────────────────────────

    #[test]
    fn docker_runtime_crud() {
        let conn = test_conn();
        let rt = DockerRuntimeRow {
            name: "colima".into(),
            socket_path: Some("~/.colima/default/docker.sock".into()),
            host: None,
            url: None,
            enabled: true,
        };
        upsert_docker_runtime(&conn, &rt).unwrap();

        let list = list_docker_runtimes(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "colima");

        let docker_host = list[0].docker_host().unwrap();
        assert!(docker_host.starts_with("unix://"), "got: {docker_host}");
        assert!(
            !docker_host.contains('~'),
            "tilde should be expanded: {docker_host}"
        );

        assert!(remove_docker_runtime(&conn, "colima").unwrap());
        assert!(list_docker_runtimes(&conn).unwrap().is_empty());
    }

    #[test]
    fn docker_runtime_tcp_host() {
        let conn = test_conn();
        let rt = DockerRuntimeRow {
            name: "remote".into(),
            socket_path: None,
            host: Some("tcp://remote:2376".into()),
            url: None,
            enabled: true,
        };
        upsert_docker_runtime(&conn, &rt).unwrap();
        let list = list_docker_runtimes(&conn).unwrap();
        assert_eq!(list[0].docker_host().as_deref(), Some("tcp://remote:2376"));
    }

    #[test]
    fn docker_runtime_web_url_no_docker_host() {
        let rt = DockerRuntimeRow {
            name: "portainer".into(),
            socket_path: None,
            host: None,
            url: Some("http://portainer:9000".into()),
            enabled: true,
        };
        assert!(
            rt.docker_host().is_none(),
            "web-only runtime should return None for docker_host"
        );
    }

    #[test]
    fn active_docker_host_returns_first_socket() {
        let conn = test_conn();
        upsert_docker_runtime(
            &conn,
            &DockerRuntimeRow {
                name: "a".into(),
                socket_path: Some("/var/run/docker.sock".into()),
                host: None,
                url: None,
                enabled: true,
            },
        )
        .unwrap();
        let host = active_docker_host(&conn).unwrap();
        assert!(host.starts_with("unix://"));
    }

    #[test]
    fn active_docker_host_none_when_empty() {
        let conn = test_conn();
        assert!(active_docker_host(&conn).is_none());
    }

    // ── OpenAPI specs ─────────────────────────────────────────────────────────

    #[test]
    fn openapi_spec_crud() {
        let conn = test_conn();
        assert!(list_openapi_specs(&conn).unwrap().is_empty());

        let spec = OpenApiSpecRow {
            name: "myapi".into(),
            url: Some("http://api.example.com/openapi.json".into()),
            source_mcp: None,
            spec_json: Some(r#"{"openapi":"3.0.0"}"#.into()),
            cached_at: Some("2026-01-01T00:00:00Z".into()),
            enabled: true,
        };
        upsert_openapi_spec(&conn, &spec).unwrap();

        let found = get_openapi_spec(&conn, "myapi").unwrap().unwrap();
        assert_eq!(
            found.url.as_deref(),
            Some("http://api.example.com/openapi.json")
        );
        assert!(found.spec_json.is_some());

        let list = list_openapi_specs(&conn).unwrap();
        assert_eq!(list.len(), 1);

        assert!(remove_openapi_spec(&conn, "myapi").unwrap());
        assert!(get_openapi_spec(&conn, "myapi").unwrap().is_none());
    }

    #[test]
    fn get_openapi_spec_returns_none_for_missing() {
        let conn = test_conn();
        assert!(get_openapi_spec(&conn, "ghost").unwrap().is_none());
    }

    // ── Plugins ───────────────────────────────────────────────────────────────

    fn make_plugin(id: &str) -> PluginRow {
        PluginRow {
            id: id.into(),
            manifest_path: format!("/plugins/{id}/manifest.toml"),
            tier: "personal".into(),
            mode: "orca".into(),
            mcp_command: Some("node".into()),
            mcp_args: vec!["server.js".into()],
            mcp_env: Default::default(),
            mcp_token_env: None,
            mcp_urls: vec![],
            context_injection: "minimal".into(),
            enabled: true,
            command_map: Default::default(),
            nav_links: vec![],
            search_tools: vec![],
            specs_dir: None,
        }
    }

    #[test]
    fn plugin_crud() {
        let conn = test_conn();
        assert!(list_plugins(&conn).unwrap().is_empty());

        upsert_plugin(&conn, &make_plugin("rebuy")).unwrap();

        let list = list_plugins(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "rebuy");
        assert_eq!(list[0].tier, "personal");

        let found = get_plugin(&conn, "rebuy").unwrap().unwrap();
        assert_eq!(found.mcp_args, vec!["server.js"]);

        assert!(remove_plugin(&conn, "rebuy").unwrap());
        assert!(list_plugins(&conn).unwrap().is_empty());
        assert!(!remove_plugin(&conn, "rebuy").unwrap());
    }

    #[test]
    fn get_plugin_returns_none_for_missing() {
        let conn = test_conn();
        assert!(get_plugin(&conn, "ghost").unwrap().is_none());
    }

    #[test]
    fn plugin_enabled_toggle() {
        let conn = test_conn();
        upsert_plugin(&conn, &make_plugin("p1")).unwrap();

        assert!(set_plugin_enabled(&conn, "p1", false).unwrap());
        let p = get_plugin(&conn, "p1").unwrap().unwrap();
        assert!(!p.enabled);

        assert!(set_plugin_enabled(&conn, "p1", true).unwrap());
        let p = get_plugin(&conn, "p1").unwrap().unwrap();
        assert!(p.enabled);

        assert!(!set_plugin_enabled(&conn, "nonexistent", true).unwrap());
    }

    #[test]
    fn plugin_deps_tracking() {
        let conn = test_conn();
        upsert_plugin(&conn, &make_plugin("parent")).unwrap();
        upsert_plugin(&conn, &make_plugin("dep-a")).unwrap();
        upsert_plugin(&conn, &make_plugin("dep-b")).unwrap();

        add_plugin_dep(&conn, "parent", "dep-a").unwrap();
        add_plugin_dep(&conn, "parent", "dep-b").unwrap();

        let deps = list_plugin_deps(&conn, "parent").unwrap();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"dep-a".to_string()));

        assert!(plugin_has_parent(&conn, "dep-a").unwrap());
        assert!(!plugin_has_parent(&conn, "parent").unwrap());

        remove_plugin_deps(&conn, "parent").unwrap();
        assert!(list_plugin_deps(&conn, "parent").unwrap().is_empty());
    }

    // ── Plugin credentials ────────────────────────────────────────────────────

    #[test]
    fn plugin_credential_set_list_delete() {
        let conn = test_conn();
        set_plugin_credential(&conn, "rebuy", "API_KEY", "secret-val").unwrap();
        set_plugin_credential(&conn, "rebuy", "OTHER", "other-val").unwrap();

        let creds = list_plugin_credentials(&conn, "rebuy").unwrap();
        assert_eq!(creds.len(), 2);
        assert!(
            creds
                .iter()
                .any(|c| c.key == "API_KEY" && c.value == "secret-val")
        );

        // Upsert resets synced_at
        set_plugin_credential(&conn, "rebuy", "API_KEY", "new-val").unwrap();
        let creds2 = list_plugin_credentials(&conn, "rebuy").unwrap();
        let api = creds2.iter().find(|c| c.key == "API_KEY").unwrap();
        assert_eq!(api.value, "new-val");
        assert!(
            api.synced_at.is_none(),
            "synced_at should be reset on update"
        );

        assert!(delete_plugin_credential(&conn, "rebuy", "API_KEY").unwrap());
        assert!(!delete_plugin_credential(&conn, "rebuy", "API_KEY").unwrap());
        assert_eq!(list_plugin_credentials(&conn, "rebuy").unwrap().len(), 1);
    }

    #[test]
    fn plugin_credentials_synced_at_set_after_mark() {
        let conn = test_conn();
        set_plugin_credential(&conn, "p", "K", "V").unwrap();
        let before = list_plugin_credentials(&conn, "p").unwrap();
        assert!(before[0].synced_at.is_none());

        super::mark_plugin_credentials_synced(&conn, "p").unwrap();
        let after = list_plugin_credentials(&conn, "p").unwrap();
        assert!(after[0].synced_at.is_some());
    }

    // ── OAuth tokens ──────────────────────────────────────────────────────────

    #[test]
    fn oauth_token_round_trip() {
        let conn = test_conn();
        assert!(get_oauth_token(&conn, "github").unwrap().is_none());

        let row = OAuthTokenRow {
            service: "github".into(),
            access_token: "gha_abc".into(),
            refresh_token: Some("refresh_xyz".into()),
            expires_at: Some("2027-01-01T00:00:00Z".into()),
        };
        upsert_oauth_token(&conn, &row).unwrap();

        let found = get_oauth_token(&conn, "github").unwrap().unwrap();
        assert_eq!(found.access_token, "gha_abc");
        assert_eq!(found.refresh_token.as_deref(), Some("refresh_xyz"));

        // Upsert updates
        let row2 = OAuthTokenRow {
            service: "github".into(),
            access_token: "new_token".into(),
            refresh_token: None,
            expires_at: None,
        };
        upsert_oauth_token(&conn, &row2).unwrap();
        let found2 = get_oauth_token(&conn, "github").unwrap().unwrap();
        assert_eq!(found2.access_token, "new_token");
        assert!(found2.refresh_token.is_none());

        assert!(delete_oauth_token(&conn, "github").unwrap());
        assert!(get_oauth_token(&conn, "github").unwrap().is_none());
        assert!(!delete_oauth_token(&conn, "github").unwrap());
    }

    // ── Plugin data ───────────────────────────────────────────────────────────

    #[test]
    fn plugin_data_set_get_list_delete() {
        let conn = test_conn();
        assert!(get_plugin_data(&conn, "p", "k").unwrap().is_none());

        set_plugin_data(&conn, "p", "key1", "val1").unwrap();
        set_plugin_data(&conn, "p", "key2", "val2").unwrap();

        let found = get_plugin_data(&conn, "p", "key1").unwrap().unwrap();
        assert_eq!(found.value, "val1");

        // Upsert
        set_plugin_data(&conn, "p", "key1", "updated").unwrap();
        assert_eq!(
            get_plugin_data(&conn, "p", "key1").unwrap().unwrap().value,
            "updated"
        );

        let list = list_plugin_data(&conn, "p").unwrap();
        assert_eq!(list.len(), 2);

        assert!(delete_plugin_data(&conn, "p", "key1").unwrap());
        assert!(!delete_plugin_data(&conn, "p", "key1").unwrap());
        assert_eq!(list_plugin_data(&conn, "p").unwrap().len(), 1);
    }

    // ── Settings ─────────────────────────────────────────────────────────────

    #[test]
    fn settings_get_set_delete() {
        let conn = test_conn();
        assert!(settings_get(&conn, "my.flag").unwrap().is_none());

        settings_set(&conn, "my.flag", "enabled").unwrap();
        assert_eq!(
            settings_get(&conn, "my.flag").unwrap().as_deref(),
            Some("enabled")
        );

        settings_set(&conn, "my.flag", "disabled").unwrap();
        assert_eq!(
            settings_get(&conn, "my.flag").unwrap().as_deref(),
            Some("disabled")
        );

        assert!(settings_delete(&conn, "my.flag").unwrap());
        assert!(settings_get(&conn, "my.flag").unwrap().is_none());
        assert!(!settings_delete(&conn, "my.flag").unwrap());
    }

    #[test]
    fn settings_list_prefix_filters() {
        let conn = test_conn();
        settings_set(&conn, "foo.a", "1").unwrap();
        settings_set(&conn, "foo.b", "2").unwrap();
        settings_set(&conn, "bar.c", "3").unwrap();

        let foo_settings = settings_list_prefix(&conn, "foo.").unwrap();
        assert_eq!(foo_settings.len(), 2);
        assert!(foo_settings.iter().all(|(k, _)| k.starts_with("foo.")));
    }

    #[test]
    fn secret_uses_settings_prefix() {
        let conn = test_conn();
        secret_set(&conn, "ANTHROPIC_KEY", "sk-ant-test").unwrap();
        // Should appear under settings with prefix
        let all = settings_list_prefix(&conn, "secrets.").unwrap();
        assert!(all.iter().any(|(k, _)| k == "secrets.ANTHROPIC_KEY"));
        // secret_get retrieves it
        assert_eq!(
            secret_get(&conn, "ANTHROPIC_KEY").unwrap().as_deref(),
            Some("sk-ant-test")
        );
        assert!(secret_delete(&conn, "ANTHROPIC_KEY").unwrap());
        assert!(secret_get(&conn, "ANTHROPIC_KEY").unwrap().is_none());
    }

    #[test]
    fn fs_allow_unrestricted_seeded_false() {
        let conn = test_conn();
        // Migration 17 seeds this as 'false'
        assert!(!fs_allow_unrestricted(&conn));
        set_setting(&conn, "fs.allow_unrestricted", "true").unwrap();
        assert!(fs_allow_unrestricted(&conn));
    }

    #[test]
    fn get_setting_vs_settings_get_both_work() {
        let conn = test_conn();
        set_setting(&conn, "x", "42").unwrap();
        assert_eq!(get_setting(&conn, "x").unwrap().as_deref(), Some("42"));
        assert_eq!(settings_get(&conn, "x").unwrap().as_deref(), Some("42"));
    }

    // ── LLM providers ─────────────────────────────────────────────────────────

    #[test]
    fn llm_provider_crud() {
        let conn = test_conn();
        assert!(list_llm_providers(&conn).unwrap().is_empty());

        upsert_llm_provider(&conn, "local", "http://localhost:1234", "lmstudio").unwrap();
        let list = list_llm_providers(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "local");
        assert!(list[0].enabled);

        // Upsert updates URL
        upsert_llm_provider(&conn, "local", "http://localhost:5678", "lmstudio").unwrap();
        let list2 = list_llm_providers(&conn).unwrap();
        assert_eq!(list2[0].url, "http://localhost:5678");

        assert!(set_llm_provider_enabled(&conn, "local", false).unwrap());
        let list3 = list_llm_providers(&conn).unwrap();
        assert!(!list3[0].enabled);

        assert!(remove_llm_provider(&conn, "local").unwrap());
        assert!(list_llm_providers(&conn).unwrap().is_empty());
        assert!(!remove_llm_provider(&conn, "local").unwrap());
    }

    // ── Doc roots ─────────────────────────────────────────────────────────────

    #[test]
    fn doc_roots_seeded_by_migration() {
        let conn = test_conn();
        let roots = list_doc_roots(&conn).unwrap();
        // Migration 15 seeds rebuy, orca, bardbase, homepage, meerkat
        assert!(
            roots.len() >= 5,
            "expected seeded doc roots, got {}",
            roots.len()
        );
        assert!(roots.iter().any(|r| r.name == "orca"));
    }

    #[test]
    fn doc_roots_crud() {
        let conn = test_conn();
        let root = DocRootRow {
            name: "myproject".into(),
            path: "/home/user/myproject".into(),
            description: Some("My project".into()),
            enabled: true,
        };
        upsert_doc_root(&conn, &root).unwrap();

        let list = list_doc_roots(&conn).unwrap();
        assert!(list.iter().any(|r| r.name == "myproject"));

        assert!(remove_doc_root(&conn, "myproject").unwrap());
        assert!(
            !list_doc_roots(&conn)
                .unwrap()
                .iter()
                .any(|r| r.name == "myproject")
        );
        assert!(!remove_doc_root(&conn, "myproject").unwrap());
    }

    // ── Doc ignore patterns ───────────────────────────────────────────────────

    #[test]
    fn doc_ignore_patterns_seeded_by_migration() {
        let conn = test_conn();
        let patterns = list_doc_ignore_patterns(&conn).unwrap();
        assert!(patterns.contains(&"node_modules".to_string()));
        assert!(patterns.contains(&".git".to_string()));
        assert!(patterns.contains(&"target".to_string()));
    }

    #[test]
    fn doc_ignore_pattern_add_remove() {
        let conn = test_conn();
        assert!(add_doc_ignore_pattern(&conn, "my_custom_dir").unwrap());
        assert!(
            !add_doc_ignore_pattern(&conn, "my_custom_dir").unwrap(),
            "duplicate insert should return false"
        );

        let patterns = list_doc_ignore_patterns(&conn).unwrap();
        assert!(patterns.contains(&"my_custom_dir".to_string()));

        assert!(remove_doc_ignore_pattern(&conn, "my_custom_dir").unwrap());
        assert!(!remove_doc_ignore_pattern(&conn, "my_custom_dir").unwrap());
    }
}
