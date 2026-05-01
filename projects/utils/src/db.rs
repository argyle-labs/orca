use anyhow::{Context, Result};
use rand::RngCore;
use rusqlite::Connection;
use std::path::Path;

/// Open (or create) the encrypted brain database.
///
/// Key is loaded from the OS keychain on first call; generated and stored if not found.
/// The database file lives at `~/brain/brain.db` by default.
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

    Ok(conn)
}

/// Open brain database using the default path (`~/brain/brain.db`).
pub fn open_default() -> Result<Connection> {
    let home = dirs::home_dir().context("no home dir")?;
    let path = home.join(".brain").join("brain.db");
    open(&path)
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
            brain_tool      TEXT PRIMARY KEY,
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
        ",
    )?;
    // Idempotent column migration for DBs created before the url column existed
    let _ = conn.execute_batch("ALTER TABLE docker_runtimes ADD COLUMN url TEXT;");
    Ok(())
}

// ── Key management ───────────────────────────────────────────────────────────

/// Load the DB encryption key from `~/.brain/.db_key`, generating it on first run.
///
/// The key file is the backup unit alongside brain.db — copy both to restore.
/// Never regenerate silently: if the file exists but is unreadable/corrupt, bail
/// so the user knows they need to restore the key rather than destroying their data.
fn load_or_create_key() -> Result<String> {
    let home = dirs::home_dir().context("no home dir")?;
    let key_path = home.join(".brain").join(".db_key");

    if key_path.exists() {
        let raw = std::fs::read_to_string(&key_path)
            .context("failed to read ~/.brain/.db_key — restore from backup or run `brain db reset` to wipe and start fresh")?;
        let key = raw.trim().to_string();
        anyhow::ensure!(
            key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()),
            "~/.brain/.db_key is corrupt (expected 64 hex chars) — restore from backup"
        );
        return Ok(key);
    }

    // First run: generate key, write with restricted permissions
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    });

    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&key_path, &hex).context("failed to write ~/.brain/.db_key")?;

    // Restrict to owner-read/write only (0600)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }

    tracing::info!("generated new DB encryption key at ~/.brain/.db_key — back this up alongside brain.db");
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

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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
    let args_json = serde_json::to_string(&server.args).unwrap_or_else(|_| "[]".into());
    let env_json = serde_json::to_string(&server.env).unwrap_or_else(|_| "{}".into());
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
        "SELECT name, host, port, user, password, database, container, domains_file, enabled
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
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn upsert_schema_database(conn: &Connection, db: &SchemaDbRow) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_databases (name, host, port, user, password, database, container, domains_file, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(name) DO UPDATE SET
             host         = excluded.host,
             port         = excluded.port,
             user         = excluded.user,
             password     = excluded.password,
             database     = excluded.database,
             container    = excluded.container,
             domains_file = excluded.domains_file,
             enabled      = excluded.enabled",
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
            let expanded = if let Some(rest) = sock.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{rest}")
            } else {
                sock.clone()
            };
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
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .ok()?;
    if let Some(sock) = socket_path {
        let expanded = if let Some(rest) = sock.strip_prefix("~/") {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/{rest}")
        } else {
            sock
        };
        Some(format!("unix://{expanded}"))
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
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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
    pub brain_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
    pub match_type: String,
    pub confidence: Option<f64>,
    pub enabled: bool,
}

pub fn list_mcp_tool_mappings(conn: &Connection, mcp_name: &str) -> Result<Vec<McpToolMappingRow>> {
    let mut stmt = conn.prepare(
        "SELECT brain_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE mcp_name = ?1 ORDER BY brain_tool",
    )?;
    let rows = stmt.query_map(rusqlite::params![mcp_name], |row| {
        Ok(McpToolMappingRow {
            brain_tool: row.get(0)?,
            mcp_name: row.get(1)?,
            external_tool: row.get(2)?,
            match_type: row.get(3)?,
            confidence: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn all_mcp_tool_mappings(conn: &Connection) -> Result<Vec<McpToolMappingRow>> {
    let mut stmt = conn.prepare(
        "SELECT brain_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE enabled = 1 ORDER BY brain_tool",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(McpToolMappingRow {
            brain_tool: row.get(0)?,
            mcp_name: row.get(1)?,
            external_tool: row.get(2)?,
            match_type: row.get(3)?,
            confidence: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn lookup_mcp_mapping(conn: &Connection, brain_tool: &str) -> Result<Option<McpToolMappingRow>> {
    let result = conn.query_row(
        "SELECT brain_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE brain_tool = ?1 AND enabled = 1",
        rusqlite::params![brain_tool],
        |row| Ok(McpToolMappingRow {
            brain_tool: row.get(0)?,
            mcp_name: row.get(1)?,
            external_tool: row.get(2)?,
            match_type: row.get(3)?,
            confidence: row.get(4)?,
            enabled: row.get::<_, i32>(5)? != 0,
        }),
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_mcp_tool_mapping(conn: &Connection, row: &McpToolMappingRow) -> Result<()> {
    conn.execute(
        "INSERT INTO mcp_tool_mappings (brain_tool, mcp_name, external_tool, match_type, confidence, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(brain_tool) DO UPDATE SET
             mcp_name      = excluded.mcp_name,
             external_tool = excluded.external_tool,
             match_type    = excluded.match_type,
             confidence    = excluded.confidence,
             enabled       = excluded.enabled",
        rusqlite::params![
            row.brain_tool, row.mcp_name, row.external_tool,
            row.match_type, row.confidence, row.enabled as i32
        ],
    )?;
    Ok(())
}

pub fn remove_mcp_tool_mapping(conn: &Connection, brain_tool: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM mcp_tool_mappings WHERE brain_tool = ?1",
        rusqlite::params![brain_tool],
    )?;
    Ok(n > 0)
}

pub fn set_mcp_tool_mapping_enabled(conn: &Connection, brain_tool: &str, enabled: bool) -> Result<bool> {
    let n = conn.execute(
        "UPDATE mcp_tool_mappings SET enabled = ?1 WHERE brain_tool = ?2",
        rusqlite::params![enabled as i32, brain_tool],
    )?;
    Ok(n > 0)
}
