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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::abi::DbReply;
    use crate::capsink::with_cap_sink;

    // ── test fixtures ────────────────────────────────────────────────────────

    /// Build a `DbRow` from `(col, value)` pairs.
    fn row(pairs: Vec<(&str, DbValue)>) -> DbRow {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    /// Serialize a `DbReply` carrying `rows` (used as the sink's canned reply).
    fn rows_reply(rows: Vec<DbRow>) -> String {
        serde_json::to_string(&DbReply { rows, affected: 0 }).unwrap()
    }

    /// Serialize a write reply reporting `affected` changed rows.
    fn affected_reply(affected: u64) -> String {
        serde_json::to_string(&DbReply {
            rows: Vec::new(),
            affected,
        })
        .unwrap()
    }

    /// Install a sink that always returns `reply_json`, run `body` under it.
    fn with_reply<R>(reply_json: String, body: impl FnOnce() -> R) -> R {
        with_cap_sink(Box::new(move |_cap, _op| Ok(reply_json.clone())), body)
    }

    /// Install a sink that records the op JSON it receives and replies with
    /// `reply_json`; returns `(result, captured_op_json)`.
    fn capture<R>(reply_json: String, body: impl FnOnce() -> R) -> (R, String) {
        let seen = Rc::new(RefCell::new(String::new()));
        let sink_seen = seen.clone();
        let out = with_cap_sink(
            Box::new(move |_cap, op| {
                *sink_seen.borrow_mut() = op.to_string();
                Ok(reply_json.clone())
            }),
            body,
        );
        let captured = seen.borrow().clone();
        (out, captured)
    }

    // ── column accessors ─────────────────────────────────────────────────────

    #[test]
    fn text_reads_text_and_errors_otherwise() {
        let r = row(vec![
            ("s", DbValue::Text("hi".into())),
            ("n", DbValue::Int(3)),
        ]);
        assert_eq!(text(&r, "s").unwrap(), "hi");
        let err = text(&r, "n").unwrap_err().to_string();
        assert!(err.contains("expected text for column 'n'"), "got: {err}");
        let missing = text(&r, "absent").unwrap_err().to_string();
        assert!(missing.contains("'absent'"), "got: {missing}");
    }

    #[test]
    fn opt_text_maps_text_null_and_absent() {
        let r = row(vec![
            ("s", DbValue::Text("v".into())),
            ("z", DbValue::Null),
            ("n", DbValue::Int(1)),
        ]);
        assert_eq!(opt_text(&r, "s"), Some("v".to_string()));
        assert_eq!(opt_text(&r, "z"), None);
        assert_eq!(opt_text(&r, "n"), None);
        assert_eq!(opt_text(&r, "absent"), None);
    }

    #[test]
    fn bool_col_accepts_bool_int_and_defaults_false() {
        let r = row(vec![
            ("b", DbValue::Bool(true)),
            ("one", DbValue::Int(1)),
            ("zero", DbValue::Int(0)),
            ("t", DbValue::Text("x".into())),
        ]);
        assert!(bool_col(&r, "b"));
        assert!(bool_col(&r, "one"));
        assert!(!bool_col(&r, "zero"));
        assert!(!bool_col(&r, "t"));
        assert!(!bool_col(&r, "absent"));
    }

    #[test]
    fn opt_real_maps_real_int_and_none() {
        let r = row(vec![
            ("f", DbValue::Real(1.5)),
            ("i", DbValue::Int(4)),
            ("z", DbValue::Null),
        ]);
        assert_eq!(opt_real(&r, "f"), Some(1.5));
        assert_eq!(opt_real(&r, "i"), Some(4.0));
        assert_eq!(opt_real(&r, "z"), None);
        assert_eq!(opt_real(&r, "absent"), None);
    }

    #[test]
    fn json_col_parses_defaults_and_absent() {
        let r = row(vec![
            ("good", DbValue::Text(r#"["a","b"]"#.into())),
            ("bad", DbValue::Text("not json".into())),
            ("z", DbValue::Null),
        ]);
        let good: Vec<String> = json_col(&r, "good");
        assert_eq!(good, vec!["a".to_string(), "b".to_string()]);
        let bad: Vec<String> = json_col(&r, "bad");
        assert!(bad.is_empty());
        let absent: Vec<String> = json_col(&r, "absent");
        assert!(absent.is_empty());
        let null: Vec<String> = json_col(&r, "z");
        assert!(null.is_empty());
    }

    // ── op builders ──────────────────────────────────────────────────────────

    #[test]
    fn op_builders_use_empty_namespace() {
        match list_op("t") {
            DbOp::List { namespace, table } => {
                assert_eq!(namespace, "");
                assert_eq!(table, "t");
            }
            other => panic!("expected List, got {other:?}"),
        }
        match get_op("t", "k", "v") {
            DbOp::Get {
                namespace,
                table,
                key_col,
                key,
            } => {
                assert_eq!(namespace, "");
                assert_eq!(table, "t");
                assert_eq!(key_col, "k");
                assert_eq!(key, "v");
            }
            other => panic!("expected Get, got {other:?}"),
        }
        match upsert_op("t", row(vec![("c", DbValue::Int(1))])) {
            DbOp::Upsert {
                namespace,
                table,
                row,
            } => {
                assert_eq!(namespace, "");
                assert_eq!(table, "t");
                assert_eq!(row.get("c"), Some(&DbValue::Int(1)));
            }
            other => panic!("expected Upsert, got {other:?}"),
        }
        match delete_op("t", "k", "v") {
            DbOp::Delete {
                namespace,
                table,
                key_col,
                key,
            } => {
                assert_eq!(namespace, "");
                assert_eq!(table, "t");
                assert_eq!(key_col, "k");
                assert_eq!(key, "v");
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    // ── mcp_servers ──────────────────────────────────────────────────────────

    fn server_row(name: &str, enabled: bool) -> DbRow {
        row(vec![
            ("name", DbValue::Text(name.into())),
            ("command", DbValue::Text("cmd".into())),
            ("args", DbValue::Text(r#"["--x"]"#.into())),
            ("env", DbValue::Text(r#"{"K":"V"}"#.into())),
            ("enabled", DbValue::Bool(enabled)),
        ])
    }

    #[test]
    fn mcp_servers_list_filters_disabled_and_sorts() {
        let reply = rows_reply(vec![
            server_row("zeta", true),
            server_row("off", false),
            server_row("alpha", true),
        ]);
        let out = with_reply(reply, mcp_servers::list).unwrap();
        let names: Vec<_> = out.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(out[0].args, vec!["--x".to_string()]);
        assert_eq!(out[0].env.get("K"), Some(&"V".to_string()));
    }

    #[test]
    fn mcp_servers_list_propagates_decode_error() {
        // `name` as Int makes `text()` fail, so the whole list errors.
        let bad = row(vec![
            ("name", DbValue::Int(1)),
            ("command", DbValue::Text("c".into())),
            ("enabled", DbValue::Bool(true)),
        ]);
        let err = with_reply(rows_reply(vec![bad]), mcp_servers::list).unwrap_err();
        assert!(err.to_string().contains("expected text"), "got: {err}");
    }

    #[test]
    fn mcp_servers_upsert_sends_upsert_op() {
        let server = mcp_servers::ServerRow {
            name: "srv".into(),
            command: "run".into(),
            args: vec!["a".into()],
            env: HashMap::new(),
            enabled: true,
        };
        let ((), op) = capture(affected_reply(1), || mcp_servers::upsert(&server).unwrap());
        assert!(op.contains(r#""op":"upsert""#), "got: {op}");
        assert!(op.contains("mcp_servers"), "got: {op}");
        assert!(op.contains(r#""srv""#), "got: {op}");
        // args is stored as a JSON string.
        assert!(op.contains(r#"[\"a\"]"#), "got: {op}");
    }

    #[test]
    fn mcp_servers_remove_reports_affected() {
        let hit = with_reply(affected_reply(1), || mcp_servers::remove("srv")).unwrap();
        assert!(hit);
        let miss = with_reply(affected_reply(0), || mcp_servers::remove("srv")).unwrap();
        assert!(!miss);
    }

    // ── tool_mappings ────────────────────────────────────────────────────────

    fn mapping_row(orca_tool: &str, mcp_name: &str, enabled: bool, conf: Option<f64>) -> DbRow {
        row(vec![
            ("orca_tool", DbValue::Text(orca_tool.into())),
            ("mcp_name", DbValue::Text(mcp_name.into())),
            ("external_tool", DbValue::Text("ext".into())),
            ("match_type", DbValue::Text("exact".into())),
            (
                "confidence",
                match conf {
                    Some(c) => DbValue::Real(c),
                    None => DbValue::Null,
                },
            ),
            ("enabled", DbValue::Bool(enabled)),
        ])
    }

    #[test]
    fn tool_mappings_list_filters_by_mcp_name_and_sorts() {
        let reply = rows_reply(vec![
            mapping_row("z", "srvA", true, Some(0.9)),
            mapping_row("a", "srvA", false, None),
            mapping_row("b", "srvB", true, None),
        ]);
        let out = with_reply(reply, || tool_mappings::list("srvA")).unwrap();
        let tools: Vec<_> = out.iter().map(|m| m.orca_tool.as_str()).collect();
        assert_eq!(tools, vec!["a", "z"]); // both srvA, sorted, no enabled filter
        assert_eq!(out[1].confidence, Some(0.9));
    }

    #[test]
    fn tool_mappings_all_filters_enabled_and_sorts() {
        let reply = rows_reply(vec![
            mapping_row("z", "srvA", true, None),
            mapping_row("a", "srvA", false, None),
        ]);
        let out = with_reply(reply, tool_mappings::all).unwrap();
        let tools: Vec<_> = out.iter().map(|m| m.orca_tool.as_str()).collect();
        assert_eq!(tools, vec!["z"]);
    }

    #[test]
    fn tool_mappings_lookup_respects_enabled_and_empty() {
        let found = with_reply(rows_reply(vec![mapping_row("t", "s", true, None)]), || {
            tool_mappings::lookup("t")
        })
        .unwrap();
        assert!(found.is_some());

        let disabled = with_reply(rows_reply(vec![mapping_row("t", "s", false, None)]), || {
            tool_mappings::lookup("t")
        })
        .unwrap();
        assert!(disabled.is_none());

        let empty = with_reply(rows_reply(vec![]), || tool_mappings::lookup("t")).unwrap();
        assert!(empty.is_none());
    }

    #[test]
    fn tool_mappings_upsert_encodes_confidence_variants() {
        let with_conf = tool_mappings::MappingRow {
            orca_tool: "t".into(),
            mcp_name: "s".into(),
            external_tool: "e".into(),
            match_type: "exact".into(),
            confidence: Some(0.5),
            enabled: true,
        };
        let ((), op) = capture(affected_reply(1), || {
            tool_mappings::upsert(&with_conf).unwrap()
        });
        assert!(op.contains(r#""t":"real""#), "got: {op}");

        let no_conf = tool_mappings::MappingRow {
            confidence: None,
            ..with_conf
        };
        let ((), op2) = capture(affected_reply(1), || {
            tool_mappings::upsert(&no_conf).unwrap()
        });
        assert!(op2.contains(r#""confidence""#), "got: {op2}");
        assert!(op2.contains(r#""t":"null""#), "got: {op2}");
    }

    #[test]
    fn tool_mappings_remove_reports_affected() {
        let hit = with_reply(affected_reply(1), || tool_mappings::remove("t")).unwrap();
        assert!(hit);
        let miss = with_reply(affected_reply(0), || tool_mappings::remove("t")).unwrap();
        assert!(!miss);
    }

    // ── openapi_specs ────────────────────────────────────────────────────────

    fn spec_row(name: &str, url: Option<&str>) -> DbRow {
        row(vec![
            ("name", DbValue::Text(name.into())),
            (
                "url",
                match url {
                    Some(u) => DbValue::Text(u.into()),
                    None => DbValue::Null,
                },
            ),
            ("source_mcp", DbValue::Null),
            ("spec_json", DbValue::Text("{}".into())),
            ("cached_at", DbValue::Null),
            ("enabled", DbValue::Bool(true)),
        ])
    }

    #[test]
    fn openapi_specs_get_some_and_none() {
        let some = with_reply(rows_reply(vec![spec_row("s", Some("http://x"))]), || {
            openapi_specs::get("s")
        })
        .unwrap();
        let got = some.unwrap();
        assert_eq!(got.name, "s");
        assert_eq!(got.url, Some("http://x".to_string()));
        assert_eq!(got.source_mcp, None);

        let none = with_reply(rows_reply(vec![]), || openapi_specs::get("s")).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn openapi_specs_list_sorts_by_name() {
        let reply = rows_reply(vec![spec_row("z", None), spec_row("a", None)]);
        let out = with_reply(reply, openapi_specs::list).unwrap();
        let names: Vec<_> = out.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "z"]);
    }

    #[test]
    fn openapi_specs_upsert_encodes_null_and_text_options() {
        let spec = openapi_specs::OpenApiSpecRow {
            name: "s".into(),
            url: Some("http://x".into()),
            source_mcp: None,
            spec_json: None,
            cached_at: None,
            enabled: false,
        };
        let ((), op) = capture(affected_reply(1), || openapi_specs::upsert(&spec).unwrap());
        assert!(op.contains(r#""op":"upsert""#), "got: {op}");
        assert!(op.contains("openapi_specs"), "got: {op}");
        assert!(op.contains("http://x"), "got: {op}");
        assert!(op.contains(r#""t":"null""#), "got: {op}");
    }

    // ── plugins ──────────────────────────────────────────────────────────────

    #[test]
    fn plugins_list_decodes_defaults_and_sorts() {
        let full = row(vec![
            ("id", DbValue::Text("zeta".into())),
            ("manifest_path", DbValue::Text("/z".into())),
            ("tier", DbValue::Text("official".into())),
            ("context_injection", DbValue::Text("ctx".into())),
            ("enabled", DbValue::Bool(true)),
            ("command_map", DbValue::Text(r#"{"a":"b"}"#.into())),
            ("specs_dir", DbValue::Text("/specs".into())),
        ]);
        // Missing/NULL tier + context_injection exercise unwrap_or_default.
        let sparse = row(vec![
            ("id", DbValue::Text("alpha".into())),
            ("manifest_path", DbValue::Text("/a".into())),
            ("tier", DbValue::Null),
            ("enabled", DbValue::Bool(false)),
            ("specs_dir", DbValue::Null),
        ]);
        let out = with_reply(rows_reply(vec![full, sparse]), plugins::list).unwrap();
        assert_eq!(out[0].id, "alpha");
        assert_eq!(out[0].tier, "");
        assert_eq!(out[0].context_injection, "");
        assert!(out[0].command_map.is_empty());
        assert_eq!(out[0].specs_dir, None);
        assert_eq!(out[1].id, "zeta");
        assert_eq!(out[1].tier, "official");
        assert_eq!(out[1].command_map.get("a"), Some(&"b".to_string()));
        assert_eq!(out[1].specs_dir, Some("/specs".to_string()));
    }

    // ── plugin_creds ─────────────────────────────────────────────────────────

    fn cred_row(plugin_id: &str, key: &str, synced: Option<&str>) -> DbRow {
        row(vec![
            ("plugin_id", DbValue::Text(plugin_id.into())),
            ("key", DbValue::Text(key.into())),
            ("value", DbValue::Text("secret".into())),
            (
                "synced_at",
                match synced {
                    Some(s) => DbValue::Text(s.into()),
                    None => DbValue::Null,
                },
            ),
            ("updated_at", DbValue::Text("now".into())),
        ])
    }

    #[test]
    fn plugin_creds_list_filters_and_sorts() {
        let reply = rows_reply(vec![
            cred_row("p1", "zkey", Some("t")),
            cred_row("p1", "akey", None),
            cred_row("p2", "other", None),
        ]);
        let out = with_reply(reply, || plugin_creds::list("p1")).unwrap();
        let keys: Vec<_> = out.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["akey", "zkey"]);
        assert_eq!(out[0].synced_at, None);
        assert_eq!(out[1].synced_at, Some("t".to_string()));
        assert_eq!(out[0].value, "secret");
        assert_eq!(out[0].updated_at, "now");
    }
}
