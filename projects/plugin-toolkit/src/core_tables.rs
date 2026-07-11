//! Typed accessors for a fixed set of orca CORE tables, for thin/subprocess
//! plugins (the MCP client is the first caller).
//!
//! Unlike a plugin's own namespaced tables, these are core-owned. Access routes
//! over the SAME capability sink as everything else — [`crate::runtime::db_op`]
//! — but with an EMPTY namespace (`""`) and a literal core table name. Core
//! resolves an empty namespace to the bare table, so a plugin reaches
//! `mcp_servers`, `plugins`, etc. through the identical FFI/cap path it already
//! uses for its own data. No rusqlite, no `db` crate: this module compiles under
//! the LIGHT `db` feature (`dep:macro-runtime` only), NOT `db-incore`.
//!
//! ## Why filtering and sorting happen in Rust
//!
//! The [`crate::abi::DbOp`] surface is intentionally tiny: `List` returns ALL
//! rows and `Get` returns a single row by `key_col == key`. It carries no
//! `WHERE`, no `ORDER BY`. The original db-crate helpers baked
//! `WHERE enabled = 1`, `WHERE mcp_name = ?`, and `ORDER BY <col>` into their
//! SQL. To preserve identical behaviour without expanding the ABI, every such
//! clause is replicated here in Rust: we `List`/`Get`, decode the rows, then
//! filter and sort the decoded `Vec` before returning.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};

use crate::abi::{DbOp, DbRow, DbValue};
use crate::runtime::db_op;

// ── Column accessors ─────────────────────────────────────────────────────────

/// Read a required TEXT column, erroring (with the column name) on any other
/// storage class.
fn text(row: &DbRow, col: &str) -> Result<String> {
    match row.get(col) {
        Some(DbValue::Text(s)) => Ok(s.clone()),
        other => bail!("expected text for column '{col}', got {other:?}"),
    }
}

/// Read an optional TEXT column: `Text` → `Some`, `Null`/absent → `None`.
fn opt_text(row: &DbRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(DbValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Read a boolean column. SQLite stores bools as INTEGER, so accept both:
/// `Bool(b)` → `b`, `Int(n)` → `n != 0`, anything else → `false`.
fn bool_col(row: &DbRow, col: &str) -> bool {
    match row.get(col) {
        Some(DbValue::Bool(b)) => *b,
        Some(DbValue::Int(n)) => *n != 0,
        _ => false,
    }
}

/// Read an optional REAL column: `Real` → `Some`, `Int` → `Some(as f64)`,
/// `Null`/absent → `None`.
fn opt_real(row: &DbRow, col: &str) -> Option<f64> {
    match row.get(col) {
        Some(DbValue::Real(f)) => Some(*f),
        Some(DbValue::Int(n)) => Some(*n as f64),
        _ => None,
    }
}

/// Decode a JSON TEXT column into `T`, treating `Null`/absent/parse-failure as
/// `T::default()`.
fn json_col<T: serde::de::DeserializeOwned + Default>(row: &DbRow, col: &str) -> T {
    match opt_text(row, col) {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => T::default(),
    }
}

/// Build an empty-namespace `List` op for a core table.
fn list_op(table: &str) -> DbOp {
    DbOp::List {
        namespace: String::new(),
        table: table.to_string(),
    }
}

/// Build an empty-namespace `Get` op for a core table.
fn get_op(table: &str, key_col: &str, key: &str) -> DbOp {
    DbOp::Get {
        namespace: String::new(),
        table: table.to_string(),
        key_col: key_col.to_string(),
        key: key.to_string(),
    }
}

/// Build an empty-namespace `Upsert` op for a core table.
fn upsert_op(table: &str, row: DbRow) -> DbOp {
    DbOp::Upsert {
        namespace: String::new(),
        table: table.to_string(),
        row,
    }
}

/// Build an empty-namespace `Delete` op for a core table.
fn delete_op(table: &str, key_col: &str, key: &str) -> DbOp {
    DbOp::Delete {
        namespace: String::new(),
        table: table.to_string(),
        key_col: key_col.to_string(),
        key: key.to_string(),
    }
}

// ── mcp_servers ───────────────────────────────────────────────────────────────

/// Configured MCP servers. Original SQL keyed on `name`; `args`/`env` are JSON.
pub mod mcp_servers {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct ServerRow {
        pub name: String,
        pub command: String,
        pub args: Vec<String>,
        pub env: HashMap<String, String>,
        pub enabled: bool,
    }

    fn decode(row: &DbRow) -> Result<ServerRow> {
        Ok(ServerRow {
            name: text(row, "name")?,
            command: text(row, "command")?,
            args: json_col(row, "args"),
            env: json_col(row, "env"),
            enabled: bool_col(row, "enabled"),
        })
    }

    /// Original: `WHERE enabled = 1 ORDER BY name`.
    pub fn list() -> Result<Vec<ServerRow>> {
        let reply = db_op(&list_op("mcp_servers"))?;
        let mut out: Vec<ServerRow> = reply
            .rows
            .iter()
            .map(decode)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|s| s.enabled)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn upsert(server: &ServerRow) -> Result<()> {
        let mut row: DbRow = BTreeMap::new();
        row.insert("name".into(), DbValue::Text(server.name.clone()));
        row.insert("command".into(), DbValue::Text(server.command.clone()));
        row.insert(
            "args".into(),
            DbValue::Text(serde_json::to_string(&server.args)?),
        );
        row.insert(
            "env".into(),
            DbValue::Text(serde_json::to_string(&server.env)?),
        );
        row.insert("enabled".into(), DbValue::Bool(server.enabled));
        db_op(&upsert_op("mcp_servers", row))?;
        Ok(())
    }

    pub fn remove(name: &str) -> Result<bool> {
        let reply = db_op(&delete_op("mcp_servers", "name", name))?;
        Ok(reply.affected > 0)
    }
}

// ── mcp_tool_mappings ──────────────────────────────────────────────────────────

/// Orca-tool → external-tool mappings per MCP server.
pub mod tool_mappings {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct MappingRow {
        pub orca_tool: String,
        pub mcp_name: String,
        pub external_tool: String,
        pub match_type: String,
        pub confidence: Option<f64>,
        pub enabled: bool,
    }

    fn decode(row: &DbRow) -> Result<MappingRow> {
        Ok(MappingRow {
            orca_tool: text(row, "orca_tool")?,
            mcp_name: text(row, "mcp_name")?,
            external_tool: text(row, "external_tool")?,
            match_type: text(row, "match_type")?,
            confidence: opt_real(row, "confidence"),
            enabled: bool_col(row, "enabled"),
        })
    }

    /// Original: `WHERE mcp_name = ? ORDER BY orca_tool` (no enabled filter).
    pub fn list(mcp_name: &str) -> Result<Vec<MappingRow>> {
        let reply = db_op(&list_op("mcp_tool_mappings"))?;
        let mut out: Vec<MappingRow> = reply
            .rows
            .iter()
            .map(decode)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|m| m.mcp_name == mcp_name)
            .collect();
        out.sort_by(|a, b| a.orca_tool.cmp(&b.orca_tool));
        Ok(out)
    }

    /// Original: `WHERE enabled = 1 ORDER BY orca_tool`.
    pub fn all() -> Result<Vec<MappingRow>> {
        let reply = db_op(&list_op("mcp_tool_mappings"))?;
        let mut out: Vec<MappingRow> = reply
            .rows
            .iter()
            .map(decode)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|m| m.enabled)
            .collect();
        out.sort_by(|a, b| a.orca_tool.cmp(&b.orca_tool));
        Ok(out)
    }

    /// Original: `WHERE orca_tool = ? AND enabled = 1`.
    pub fn lookup(orca_tool: &str) -> Result<Option<MappingRow>> {
        let reply = db_op(&get_op("mcp_tool_mappings", "orca_tool", orca_tool))?;
        if let Some(row) = reply.rows.first() {
            let m = decode(row)?;
            if m.enabled {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    pub fn upsert(mapping: &MappingRow) -> Result<()> {
        let mut row: DbRow = BTreeMap::new();
        row.insert("orca_tool".into(), DbValue::Text(mapping.orca_tool.clone()));
        row.insert("mcp_name".into(), DbValue::Text(mapping.mcp_name.clone()));
        row.insert(
            "external_tool".into(),
            DbValue::Text(mapping.external_tool.clone()),
        );
        row.insert(
            "match_type".into(),
            DbValue::Text(mapping.match_type.clone()),
        );
        row.insert(
            "confidence".into(),
            match mapping.confidence {
                Some(c) => DbValue::Real(c),
                None => DbValue::Null,
            },
        );
        row.insert("enabled".into(), DbValue::Bool(mapping.enabled));
        db_op(&upsert_op("mcp_tool_mappings", row))?;
        Ok(())
    }

    pub fn remove(orca_tool: &str) -> Result<bool> {
        let reply = db_op(&delete_op("mcp_tool_mappings", "orca_tool", orca_tool))?;
        Ok(reply.affected > 0)
    }
}

// ── openapi_specs ──────────────────────────────────────────────────────────────

/// Cached OpenAPI specs keyed on `name`.
pub mod openapi_specs {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct OpenApiSpecRow {
        pub name: String,
        pub url: Option<String>,
        pub source_mcp: Option<String>,
        pub spec_json: Option<String>,
        pub cached_at: Option<String>,
        pub enabled: bool,
    }

    fn decode(row: &DbRow) -> Result<OpenApiSpecRow> {
        Ok(OpenApiSpecRow {
            name: text(row, "name")?,
            url: opt_text(row, "url"),
            source_mcp: opt_text(row, "source_mcp"),
            spec_json: opt_text(row, "spec_json"),
            cached_at: opt_text(row, "cached_at"),
            enabled: bool_col(row, "enabled"),
        })
    }

    /// Original `get`: no enabled filter.
    pub fn get(name: &str) -> Result<Option<OpenApiSpecRow>> {
        let reply = db_op(&get_op("openapi_specs", "name", name))?;
        match reply.rows.first() {
            Some(row) => Ok(Some(decode(row)?)),
            None => Ok(None),
        }
    }

    pub fn list() -> Result<Vec<OpenApiSpecRow>> {
        let reply = db_op(&list_op("openapi_specs"))?;
        let mut out: Vec<OpenApiSpecRow> =
            reply.rows.iter().map(decode).collect::<Result<Vec<_>>>()?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn upsert(spec: &OpenApiSpecRow) -> Result<()> {
        let mut row: DbRow = BTreeMap::new();
        row.insert("name".into(), DbValue::Text(spec.name.clone()));
        row.insert("url".into(), opt(&spec.url));
        row.insert("source_mcp".into(), opt(&spec.source_mcp));
        row.insert("spec_json".into(), opt(&spec.spec_json));
        row.insert("cached_at".into(), opt(&spec.cached_at));
        row.insert("enabled".into(), DbValue::Bool(spec.enabled));
        db_op(&upsert_op("openapi_specs", row))?;
        Ok(())
    }

    fn opt(v: &Option<String>) -> DbValue {
        match v {
            Some(s) => DbValue::Text(s.clone()),
            None => DbValue::Null,
        }
    }
}

// ── plugins ────────────────────────────────────────────────────────────────────

/// Registered plugins. `command_map` is a JSON object; `context_injection` and
/// `specs_dir` may be NULL.
pub mod plugins {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct PluginRow {
        pub id: String,
        pub manifest_path: String,
        pub tier: String,
        pub context_injection: String,
        pub enabled: bool,
        pub command_map: HashMap<String, String>,
        pub specs_dir: Option<String>,
    }

    fn decode(row: &DbRow) -> Result<PluginRow> {
        Ok(PluginRow {
            id: text(row, "id")?,
            manifest_path: text(row, "manifest_path")?,
            tier: opt_text(row, "tier").unwrap_or_default(),
            context_injection: opt_text(row, "context_injection").unwrap_or_default(),
            enabled: bool_col(row, "enabled"),
            command_map: json_col(row, "command_map"),
            specs_dir: opt_text(row, "specs_dir"),
        })
    }

    /// Original: `ORDER BY id` (no enabled filter — callers filter themselves).
    pub fn list() -> Result<Vec<PluginRow>> {
        let reply = db_op(&list_op("plugins"))?;
        let mut out: Vec<PluginRow> = reply.rows.iter().map(decode).collect::<Result<Vec<_>>>()?;
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

// ── plugin_credentials ─────────────────────────────────────────────────────────

/// Per-plugin credentials.
pub mod plugin_creds {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct CredentialRow {
        pub plugin_id: String,
        pub key: String,
        pub value: String,
        pub synced_at: Option<String>,
        pub updated_at: String,
    }

    fn decode(row: &DbRow) -> Result<CredentialRow> {
        Ok(CredentialRow {
            plugin_id: text(row, "plugin_id")?,
            key: text(row, "key")?,
            value: text(row, "value")?,
            synced_at: opt_text(row, "synced_at"),
            updated_at: text(row, "updated_at")?,
        })
    }

    /// Original: `WHERE plugin_id = ? ORDER BY key`.
    pub fn list(plugin_id: &str) -> Result<Vec<CredentialRow>> {
        let reply = db_op(&list_op("plugin_credentials"))?;
        let mut out: Vec<CredentialRow> = reply
            .rows
            .iter()
            .map(decode)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|c| c.plugin_id == plugin_id)
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}
