//! Encrypted SQLite database (`orca.db`) — the runtime registry for all dynamic orca config.
//!
//! `open_default()` is the standard entry point. It opens (or creates) `~/.orca/orca.db`,
//! applies the SQLCipher encryption key, runs `apply_schema` to ensure all tables exist,
//! then applies any pending schema migrations via `run_pending_migrations`.
//!
//! Adding a new registry feature means adding a table in `apply_schema`, CRUD helpers at
//! the bottom of this file, and a migration entry in `MIGRATIONS` if the table was added
//! to an already-deployed database.

pub mod docker_runtimes;
pub mod home_assistant;
pub mod mcp_servers;
pub mod oauth;
pub mod openapi_specs;
pub mod plugin_creds;
pub mod plugin_data;
pub mod plugins;
pub mod proxmox;
pub mod schema_databases;
pub mod startup;
pub mod tool_mappings;

use anyhow::{Context, Result};
use config::{APP_DB_FILE, APP_STATE_DIR};
use rand::RngCore;
use rusqlite::Connection;
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

    ensure_schema_once(&conn, path)?;

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
    ensure_schema_once(&conn, path)?;
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
    Migration {
        version: 19,
        description: "add profiles, profile_shares, user_active_profile, profile_credentials — multi-profile + sharing",
        up: "CREATE TABLE IF NOT EXISTS profiles (
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
        );",
        down: Some(
            "DROP TABLE IF EXISTS profile_credentials;
                    DROP TABLE IF EXISTS user_active_profile;
                    DROP TABLE IF EXISTS profile_shares;
                    DROP TABLE IF EXISTS profiles;",
        ),
    },
    Migration {
        version: 20,
        description: "add plugin_tools — per-plugin tool registry declared via orca/tools.declare",
        up: "CREATE TABLE IF NOT EXISTS plugin_tools (
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
        CREATE INDEX IF NOT EXISTS idx_plugin_tools_fq ON plugin_tools(fq_name);",
        down: Some("DROP TABLE IF EXISTS plugin_tools;"),
    },
    Migration {
        version: 21,
        description: "add plugin_installs — per-system version channel + lock for meerkat-managed plugins",
        // system_id is the orca node identity (pod mesh). Until that lands,
        // single-system installs use 'local' (mirroring config::LOCAL_USER).
        // channel constrains version selection: 'latest' tracks stable, 'latest-rc'
        // tracks pre-release, 'locked' pins to locked_version exactly.
        // desired_version is the last resolved version for the channel; the
        // file-sync layer reconciles installed_version on disk to match.
        up: "CREATE TABLE IF NOT EXISTS plugin_installs (
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
        CREATE INDEX IF NOT EXISTS idx_plugin_installs_plugin ON plugin_installs(plugin_id);",
        down: Some("DROP TABLE IF EXISTS plugin_installs;"),
    },
    Migration {
        version: 22,
        description: "add proxmox_endpoints — registered Proxmox VE clusters with API-token auth",
        up: "CREATE TABLE IF NOT EXISTS proxmox_endpoints (
            name         TEXT PRIMARY KEY,
            base_url     TEXT NOT NULL,
            token_id     TEXT NOT NULL,
            token_secret TEXT NOT NULL,
            insecure     INTEGER NOT NULL DEFAULT 0,
            enabled      INTEGER NOT NULL DEFAULT 1,
            created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );",
        down: Some("DROP TABLE IF EXISTS proxmox_endpoints;"),
    },
    Migration {
        version: 23,
        description: "add homeassistant_endpoints — registered Home Assistant instances with bearer-token auth",
        up: "CREATE TABLE IF NOT EXISTS homeassistant_endpoints (
            name       TEXT PRIMARY KEY,
            base_url   TEXT NOT NULL,
            token      TEXT NOT NULL,
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );",
        down: Some("DROP TABLE IF EXISTS homeassistant_endpoints;"),
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
                tracing::debug!("nothing to migrate (schema at v{current})");
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
                tracing::debug!("nothing to roll back (schema at v{current})");
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

// ── plugin_tools CRUD ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginToolRow {
    pub plugin_id: String,
    pub name: String,
    pub fq_name: String,
    pub description: String,
    /// Raw JSON Schema text describing the tool's input arguments.
    pub input_schema: String,
    /// "general" | "sensitive"
    pub sensitivity: String,
    pub declared_at: String,
}

/// Upsert a plugin-declared tool. Fully-qualified name is `<plugin_id>.<name>`
/// and is unique across all plugins. Re-declaring the same `(plugin_id, name)`
/// updates the description / schema / sensitivity in place.
pub fn upsert_plugin_tool(
    conn: &Connection,
    plugin_id: &str,
    name: &str,
    description: &str,
    input_schema: &str,
    sensitivity: &str,
) -> Result<()> {
    if !matches!(sensitivity, "general" | "sensitive") {
        anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
    }
    let fq = format!("{plugin_id}.{name}");
    conn.execute(
        "INSERT INTO plugin_tools
            (plugin_id, name, fq_name, description, input_schema, sensitivity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(plugin_id, name) DO UPDATE SET
            description    = excluded.description,
            input_schema   = excluded.input_schema,
            sensitivity    = excluded.sensitivity,
            declared_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![plugin_id, name, fq, description, input_schema, sensitivity],
    )?;
    Ok(())
}

/// Replace the entire tool set for a plugin. The host invokes this when
/// `orca/tools.declare` arrives — declarations are idempotent and replace
/// the previously-known set, so any tool the plugin no longer declares is
/// removed from the registry.
pub fn replace_plugin_tools(
    conn: &mut Connection,
    plugin_id: &str,
    tools: &[(String, String, String, String)],
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM plugin_tools WHERE plugin_id = ?1", [plugin_id])?;
    for (name, description, schema, sensitivity) in tools {
        if !matches!(sensitivity.as_str(), "general" | "sensitive") {
            anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
        }
        let fq = format!("{plugin_id}.{name}");
        tx.execute(
            "INSERT INTO plugin_tools
                (plugin_id, name, fq_name, description, input_schema, sensitivity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![plugin_id, name, fq, description, schema, sensitivity],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// List all tools declared by a single plugin.
pub fn list_plugin_tools(conn: &Connection, plugin_id: &str) -> Result<Vec<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools WHERE plugin_id = ?1 ORDER BY name",
    )?;
    let rows = stmt.query_map([plugin_id], |r| {
        Ok(PluginToolRow {
            plugin_id: r.get(0)?,
            name: r.get(1)?,
            fq_name: r.get(2)?,
            description: r.get(3)?,
            input_schema: r.get(4)?,
            sensitivity: r.get(5)?,
            declared_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// List every tool across every plugin — what the MCP registry needs to
/// surface plugin tools to LLMs.
pub fn list_all_plugin_tools(conn: &Connection) -> Result<Vec<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools ORDER BY fq_name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PluginToolRow {
            plugin_id: r.get(0)?,
            name: r.get(1)?,
            fq_name: r.get(2)?,
            description: r.get(3)?,
            input_schema: r.get(4)?,
            sensitivity: r.get(5)?,
            declared_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up a single tool by its fully-qualified name (`<plugin_id>.<name>`).
pub fn get_plugin_tool(conn: &Connection, fq_name: &str) -> Result<Option<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools WHERE fq_name = ?1",
    )?;
    let row = stmt
        .query_row([fq_name], |r| {
            Ok(PluginToolRow {
                plugin_id: r.get(0)?,
                name: r.get(1)?,
                fq_name: r.get(2)?,
                description: r.get(3)?,
                input_schema: r.get(4)?,
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

// ── plugin_installs CRUD ─────────────────────────────────────────────────────

/// Identity placeholder for the local orca node before pod-mesh node identity
/// lands. Mirrors `config::LOCAL_USER` — once each node has a real id, this
/// constant goes away and callers pass the actual `system_id`.
pub const LOCAL_SYSTEM: &str = "local";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginInstallRow {
    pub system_id: String,
    pub plugin_id: String,
    /// "latest" | "latest-rc" | "locked"
    pub channel: String,
    /// Required when `channel == "locked"`; ignored otherwise.
    pub locked_version: Option<String>,
    /// Last resolved version for the channel. The reconciler uses this as
    /// the target for binary sync.
    pub desired_version: Option<String>,
    /// Version actually present on disk; None until the reconciler reports
    /// success.
    pub installed_version: Option<String>,
    pub installed_at: Option<String>,
    pub updated_at: String,
}

/// Set or update the install record for `(system_id, plugin_id)`. Channel +
/// lock policy may be changed at any time; the reconciler picks up changes
/// on its next pass and pulls the matching binary.
pub fn upsert_plugin_install(
    conn: &Connection,
    system_id: &str,
    plugin_id: &str,
    channel: &str,
    locked_version: Option<&str>,
) -> Result<()> {
    if !matches!(channel, "latest" | "latest-rc" | "locked") {
        anyhow::bail!("channel must be 'latest', 'latest-rc' or 'locked', got '{channel}'");
    }
    if channel == "locked" && locked_version.is_none() {
        anyhow::bail!("locked_version is required when channel = 'locked'");
    }
    conn.execute(
        "INSERT INTO plugin_installs
            (system_id, plugin_id, channel, locked_version, updated_at)
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(system_id, plugin_id) DO UPDATE SET
            channel        = excluded.channel,
            locked_version = excluded.locked_version,
            updated_at     = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![system_id, plugin_id, channel, locked_version],
    )?;
    Ok(())
}

/// Record reconciliation progress: the reconciler resolved `desired_version`
/// and (if successful) installed `installed_version`. `installed_version=None`
/// is allowed — used to mark "resolved but not yet downloaded".
pub fn set_plugin_install_versions(
    conn: &Connection,
    system_id: &str,
    plugin_id: &str,
    desired_version: Option<&str>,
    installed_version: Option<&str>,
) -> Result<bool> {
    let installed_at = if installed_version.is_some() {
        "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    } else {
        "installed_at"
    };
    let sql = format!(
        "UPDATE plugin_installs SET
            desired_version   = ?3,
            installed_version = ?4,
            installed_at      = {installed_at},
            updated_at        = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE system_id = ?1 AND plugin_id = ?2"
    );
    let n = conn.execute(
        &sql,
        rusqlite::params![system_id, plugin_id, desired_version, installed_version],
    )?;
    Ok(n > 0)
}

/// Remove the install record for `(system_id, plugin_id)`. Returns true if a
/// row was deleted. Does not delete the actual binary — that's the file-sync
/// layer's job to reconcile.
pub fn delete_plugin_install(conn: &Connection, system_id: &str, plugin_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM plugin_installs WHERE system_id = ?1 AND plugin_id = ?2",
        rusqlite::params![system_id, plugin_id],
    )?;
    Ok(n > 0)
}

/// Fetch the install record for `(system_id, plugin_id)`, or None.
pub fn get_plugin_install(
    conn: &Connection,
    system_id: &str,
    plugin_id: &str,
) -> Result<Option<PluginInstallRow>> {
    let mut stmt = conn.prepare(
        "SELECT system_id, plugin_id, channel, locked_version, desired_version,
                installed_version, installed_at, updated_at
         FROM plugin_installs WHERE system_id = ?1 AND plugin_id = ?2",
    )?;
    let row = stmt
        .query_row(rusqlite::params![system_id, plugin_id], |r| {
            Ok(PluginInstallRow {
                system_id: r.get(0)?,
                plugin_id: r.get(1)?,
                channel: r.get(2)?,
                locked_version: r.get(3)?,
                desired_version: r.get(4)?,
                installed_version: r.get(5)?,
                installed_at: r.get(6)?,
                updated_at: r.get(7)?,
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

/// List every install record for `system_id`. Used by the reconciler to walk
/// the desired plugin set for this node.
pub fn list_plugin_installs(conn: &Connection, system_id: &str) -> Result<Vec<PluginInstallRow>> {
    let mut stmt = conn.prepare(
        "SELECT system_id, plugin_id, channel, locked_version, desired_version,
                installed_version, installed_at, updated_at
         FROM plugin_installs WHERE system_id = ?1 ORDER BY plugin_id",
    )?;
    let rows = stmt.query_map([system_id], |r| {
        Ok(PluginInstallRow {
            system_id: r.get(0)?,
            plugin_id: r.get(1)?,
            channel: r.get(2)?,
            locked_version: r.get(3)?,
            desired_version: r.get(4)?,
            installed_version: r.get(5)?,
            installed_at: r.get(6)?,
            updated_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn fs_allow_unrestricted(conn: &Connection) -> bool {
    get_setting(conn, "fs.allow_unrestricted")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false)
}

// ── Profiles ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProfileRow {
    pub id: String,
    pub name: String,
    pub owner_user_id: String,
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ProfileShareRow {
    pub profile_id: String,
    pub user_id: String,
    pub role: String,
    pub shared_at: String,
}

/// Insert a new profile. Returns Err if (owner_user_id, name) is taken.
pub fn create_profile(
    conn: &Connection,
    id: &str,
    name: &str,
    owner_user_id: &str,
    description: Option<&str>,
) -> Result<ProfileRow> {
    conn.execute(
        "INSERT INTO profiles (id, name, owner_user_id, description) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, name, owner_user_id, description],
    )?;
    get_profile(conn, id)?.ok_or_else(|| anyhow::anyhow!("profile vanished after insert: {id}"))
}

pub fn get_profile(conn: &Connection, id: &str) -> Result<Option<ProfileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, owner_user_id, description, created_at, updated_at
         FROM profiles WHERE id = ?1",
    )?;
    let row = stmt
        .query_row([id], |r| {
            Ok(ProfileRow {
                id: r.get(0)?,
                name: r.get(1)?,
                owner_user_id: r.get(2)?,
                description: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
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

/// Find a profile owned by `owner_user_id` with the given `name`.
pub fn get_profile_by_owner_and_name(
    conn: &Connection,
    owner_user_id: &str,
    name: &str,
) -> Result<Option<ProfileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, owner_user_id, description, created_at, updated_at
         FROM profiles WHERE owner_user_id = ?1 AND name = ?2",
    )?;
    let row = stmt
        .query_row(rusqlite::params![owner_user_id, name], |r| {
            Ok(ProfileRow {
                id: r.get(0)?,
                name: r.get(1)?,
                owner_user_id: r.get(2)?,
                description: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
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

/// All profiles a user can access — owned + shared (any role).
pub fn list_profiles_for_user(conn: &Connection, user_id: &str) -> Result<Vec<ProfileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, owner_user_id, description, created_at, updated_at FROM profiles
         WHERE owner_user_id = ?1
            OR id IN (SELECT profile_id FROM profile_shares WHERE user_id = ?1)
         ORDER BY name",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok(ProfileRow {
            id: r.get(0)?,
            name: r.get(1)?,
            owner_user_id: r.get(2)?,
            description: r.get(3)?,
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn update_profile(
    conn: &Connection,
    id: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE profiles
            SET name        = COALESCE(?2, name),
                description = COALESCE(?3, description),
                updated_at  = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
          WHERE id = ?1",
        rusqlite::params![id, name, description],
    )?;
    Ok(n > 0)
}

pub fn delete_profile(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM profiles WHERE id = ?1", [id])?;
    Ok(n > 0)
}

/// Share or update a share role. Owner cannot self-share (caller enforces).
pub fn share_profile(conn: &Connection, profile_id: &str, user_id: &str, role: &str) -> Result<()> {
    if role != "viewer" && role != "collaborator" {
        return Err(anyhow::anyhow!(
            "invalid role '{role}' (expected 'viewer' or 'collaborator')"
        ));
    }
    conn.execute(
        "INSERT INTO profile_shares (profile_id, user_id, role) VALUES (?1, ?2, ?3)
         ON CONFLICT(profile_id, user_id) DO UPDATE SET
             role      = excluded.role,
             shared_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![profile_id, user_id, role],
    )?;
    Ok(())
}

pub fn unshare_profile(conn: &Connection, profile_id: &str, user_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM profile_shares WHERE profile_id = ?1 AND user_id = ?2",
        rusqlite::params![profile_id, user_id],
    )?;
    Ok(n > 0)
}

pub fn list_profile_shares(conn: &Connection, profile_id: &str) -> Result<Vec<ProfileShareRow>> {
    let mut stmt = conn.prepare(
        "SELECT profile_id, user_id, role, shared_at
         FROM profile_shares WHERE profile_id = ?1 ORDER BY user_id",
    )?;
    let rows = stmt.query_map([profile_id], |r| {
        Ok(ProfileShareRow {
            profile_id: r.get(0)?,
            user_id: r.get(1)?,
            role: r.get(2)?,
            shared_at: r.get(3)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Get the role a user has on a profile: 'owner', 'viewer', 'collaborator', or None.
pub fn profile_role_for_user(
    conn: &Connection,
    profile_id: &str,
    user_id: &str,
) -> Result<Option<String>> {
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner_user_id FROM profiles WHERE id = ?1",
            [profile_id],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
    if owner.as_deref() == Some(user_id) {
        return Ok(Some("owner".to_string()));
    }
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM profile_shares WHERE profile_id = ?1 AND user_id = ?2",
            rusqlite::params![profile_id, user_id],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
    Ok(role)
}

pub fn set_active_profile(conn: &Connection, user_id: &str, profile_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO user_active_profile (user_id, profile_id) VALUES (?1, ?2)
         ON CONFLICT(user_id) DO UPDATE SET
             profile_id = excluded.profile_id,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![user_id, profile_id],
    )?;
    Ok(())
}

pub fn get_active_profile(conn: &Connection, user_id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT profile_id FROM user_active_profile WHERE user_id = ?1",
        [user_id],
        |r| r.get::<_, String>(0),
    )
    .map(Some)
    .or_else(|e| {
        if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            Ok(None)
        } else {
            Err(e)
        }
    })
    .map_err(Into::into)
}

// ── Profile credentials ───────────────────────────────────────────────────────

pub fn set_profile_credential(
    conn: &Connection,
    profile_id: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO profile_credentials (profile_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(profile_id, key) DO UPDATE SET
             value      = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![profile_id, key, value],
    )?;
    Ok(())
}

pub fn get_profile_credential(
    conn: &Connection,
    profile_id: &str,
    key: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM profile_credentials WHERE profile_id = ?1 AND key = ?2",
        rusqlite::params![profile_id, key],
        |r| r.get::<_, String>(0),
    )
    .map(Some)
    .or_else(|e| {
        if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            Ok(None)
        } else {
            Err(e)
        }
    })
    .map_err(Into::into)
}

pub fn list_profile_credentials(conn: &Connection, profile_id: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT key FROM profile_credentials WHERE profile_id = ?1 ORDER BY key")?;
    let rows = stmt.query_map([profile_id], |r| r.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn delete_profile_credential(conn: &Connection, profile_id: &str, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM profile_credentials WHERE profile_id = ?1 AND key = ?2",
        rusqlite::params![profile_id, key],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Open an unencrypted in-memory database with full schema + migrations applied.
    pub fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        conn.execute_batch("PRAGMA journal_mode = WAL;").ok();
        conn.execute_batch("PRAGMA foreign_keys = ON;").ok();
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

    // ── plugin_installs ──────────────────────────────────────────────────────

    #[test]
    fn plugin_install_upsert_get_list_delete() {
        let conn = test_conn();

        upsert_plugin_install(&conn, LOCAL_SYSTEM, "dockge", "latest", None).unwrap();
        upsert_plugin_install(
            &conn,
            LOCAL_SYSTEM,
            "homeassistant",
            "locked",
            Some("0.3.1"),
        )
        .unwrap();

        let row = get_plugin_install(&conn, LOCAL_SYSTEM, "dockge")
            .unwrap()
            .unwrap();
        assert_eq!(row.channel, "latest");
        assert!(row.locked_version.is_none());
        assert!(row.installed_version.is_none());

        let locked = get_plugin_install(&conn, LOCAL_SYSTEM, "homeassistant")
            .unwrap()
            .unwrap();
        assert_eq!(locked.channel, "locked");
        assert_eq!(locked.locked_version.as_deref(), Some("0.3.1"));

        // Reconciler reports installed version.
        let updated = set_plugin_install_versions(
            &conn,
            LOCAL_SYSTEM,
            "dockge",
            Some("0.0.1-alpha.1"),
            Some("0.0.1-alpha.1"),
        )
        .unwrap();
        assert!(updated);
        let row = get_plugin_install(&conn, LOCAL_SYSTEM, "dockge")
            .unwrap()
            .unwrap();
        assert_eq!(row.installed_version.as_deref(), Some("0.0.1-alpha.1"));
        assert!(row.installed_at.is_some());

        let all = list_plugin_installs(&conn, LOCAL_SYSTEM).unwrap();
        assert_eq!(all.len(), 2);

        // Switching channel back to latest should clear lock requirement.
        upsert_plugin_install(&conn, LOCAL_SYSTEM, "homeassistant", "latest-rc", None).unwrap();
        let row = get_plugin_install(&conn, LOCAL_SYSTEM, "homeassistant")
            .unwrap()
            .unwrap();
        assert_eq!(row.channel, "latest-rc");
        assert!(row.locked_version.is_none());

        assert!(delete_plugin_install(&conn, LOCAL_SYSTEM, "dockge").unwrap());
        assert!(!delete_plugin_install(&conn, LOCAL_SYSTEM, "dockge").unwrap());
    }

    #[test]
    fn plugin_install_rejects_invalid_channel() {
        let conn = test_conn();
        let err = upsert_plugin_install(&conn, LOCAL_SYSTEM, "dockge", "stable", None).unwrap_err();
        assert!(err.to_string().contains("channel"));
    }

    #[test]
    fn plugin_install_locked_requires_version() {
        let conn = test_conn();
        let err = upsert_plugin_install(&conn, LOCAL_SYSTEM, "dockge", "locked", None).unwrap_err();
        assert!(err.to_string().contains("locked_version"));
    }

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
