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
pub mod docs;
pub mod home_assistant;
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
pub mod profile_creds;
pub mod profiles;
pub mod proxmox;
pub mod schema_databases;
pub mod settings;
pub mod startup;
pub mod tool_mappings;

use anyhow::{Context, Result};
use config::{APP_DB_FILE, APP_STATE_DIR};
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

    // First run: generate key, write with restricted permissions.
    // rand 0.9 moved OsRng's RngCore impl behind TryRngCore.
    use rand::TryRngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow::anyhow!("OsRng failure generating db key: {e}"))?;
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
