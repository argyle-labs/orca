//! Daemon-side capability host — the counterpart to a subprocess plugin's
//! capability sink (`plugin_toolkit::serve`).
//!
//! When the supervisor reads a [`Cap`](plugin_proto::Frame::Cap) frame from a
//! plugin, it calls [`handle_cap`] to execute the request against orca's own
//! services and returns the result as a
//! [`CapResult`](plugin_proto::Frame::CapResult). A plugin thus delegates DB /
//! secret access (and, later, HTTP / transport) instead of linking its own —
//! the whole point of the thin-plugin model.
//!
//! These route to the SAME pooled executors the in-process toolkit falls back
//! to (`db::plugin_tables::exec_db_op_pooled` / `secrets::exec_secret_op_pooled`),
//! so a tool behaves identically whether its plugin is loaded in-process or run
//! as a subprocess.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use plugin_toolkit::abi::{
    AgentRegistration, DbOp, DbReply, DbRow, DbValue, HttpRequest, HttpResponse, HttpStreamChunk,
    HttpStreamRequest, SecretOp,
};
use plugin_toolkit::serde_json::{self, Value};

/// Capability names the daemon serves. Advertised in the handshake `Welcome`.
/// `http.stream` is the streaming sibling of `http.request`: same request shape,
/// but the response body is relayed chunk-by-chunk instead of buffered.
pub const CAPABILITIES: &[&str] = &[
    "db.op",
    "secret.op",
    "http.request",
    "http.stream",
    "agents.register",
];

/// Whether `cap` is a STREAMING capability — one the supervisor drives through
/// [`handle_cap_stream`] (emitting `CapStreamChunk`/`CapStreamEnd`) rather than
/// the one-shot [`handle_cap`] (`CapResult`).
pub fn is_streaming_cap(cap: &str) -> bool {
    cap == "http.stream"
}

/// A small dedicated runtime for capability I/O (`http.request`). `handle_cap`
/// is synchronous and runs on the supervisor's blocking invoke thread (see
/// `plugin_loader::dispatch`, which drives plugin invokes via `spawn_blocking`),
/// never on a daemon async worker — so blocking on this runtime is safe and
/// keeps capability HTTP off the main scheduler.
fn cap_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build capability I/O runtime")
    })
}

/// The daemon's shared HTTP client for capability requests.
fn http_client() -> &'static utils::http::Client {
    static CLIENT: OnceLock<utils::http::Client> = OnceLock::new();
    CLIENT.get_or_init(utils::http::Client::new)
}

/// A secret `name` belongs to `principal` when it is exactly the principal or is
/// scoped beneath it (`<principal>.…`). This matches the toolkit convention that
/// a plugin's secrets are named `<provider>.<instance>.<field>` with `provider`
/// == the plugin id (`plugin_toolkit::secrets::scoped_name`).
fn secret_name_owned(name: &str, principal: &str) -> bool {
    name == principal || name.starts_with(&format!("{principal}."))
}

/// An agents provider name belongs to `principal` when it is the plugin id or its
/// owner-qualified form (`argyle-labs/agents` for plugin `agents`). Provider
/// names key the registry, so this stops one plugin replacing another's roster.
pub(crate) fn agent_provider_owned(name: &str, principal: &str) -> bool {
    name == principal
        || name
            .rsplit_once('/')
            .is_some_and(|(owner, id)| !owner.is_empty() && id == principal)
}

/// Access a plugin has to a core table addressed with the empty namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreTableAccess {
    /// Shared by all plugins; rows are confined to `provider == principal`.
    SharedByProvider,
    ReadWrite,
    ReadOnly,
}

/// Default-deny policy for `db.op` with namespace "": a core table is reachable
/// only if listed here, by its owner (`None` = every plugin). Everything else
/// (settings, secrets, tokens, mesh, users, …) is refused.
const CORE_TABLE_POLICY: &[(&str, Option<&str>, CoreTableAccess)] = &[
    ("endpoints", None, CoreTableAccess::SharedByProvider),
    ("ntfy_endpoints", Some("ntfy"), CoreTableAccess::ReadWrite),
    ("mcp_servers", Some("mcp"), CoreTableAccess::ReadWrite),
    ("mcp_tool_mappings", Some("mcp"), CoreTableAccess::ReadWrite),
    ("openapi_specs", Some("mcp"), CoreTableAccess::ReadWrite),
    ("plugins", Some("mcp"), CoreTableAccess::ReadOnly),
    // Temporary: mcp reads credentials directly until a dedicated capability replaces this.
    ("plugin_credentials", Some("mcp"), CoreTableAccess::ReadOnly),
];

fn core_table_access(table: &str, principal: &str) -> Option<CoreTableAccess> {
    CORE_TABLE_POLICY
        .iter()
        .find(|(t, owner, _)| *t == table && owner.is_none_or(|o| o == principal))
        .map(|(_, _, access)| *access)
}

fn deny(principal: &str, op: &DbOp, table: &str, why: &str) -> anyhow::Error {
    tracing::warn!(
        target: "plugin",
        plugin = %principal,
        op = %op.kind(),
        namespace = %op.namespace(),
        table = %table,
        "db.op refused: {why}"
    );
    anyhow!(
        "plugin '{principal}' may not db.op/{} on '{table}' (namespace '{}'): {why}",
        op.kind(),
        op.namespace()
    )
}

fn op_table(op: &DbOp) -> &str {
    match op {
        DbOp::List { table, .. }
        | DbOp::Get { table, .. }
        | DbOp::Insert { table, .. }
        | DbOp::Update { table, .. }
        | DbOp::Upsert { table, .. }
        | DbOp::Delete { table, .. } => table,
    }
}

fn row_provider_is(row: &DbRow, principal: &str) -> bool {
    matches!(row.get("provider"), Some(DbValue::Text(p)) if p == principal)
}

/// Every existing row in `table` whose `key_col` equals `key` must belong to
/// `principal` — guards Update/Delete/Upsert from touching a foreign row.
fn existing_rows_owned(
    conn: &db::Conn,
    table: &str,
    key_col: &str,
    key: &str,
    principal: &str,
) -> Result<bool> {
    let existing = db::plugin_tables::exec_db_op(
        conn,
        &DbOp::Get {
            namespace: String::new(),
            table: table.to_string(),
            key_col: key_col.to_string(),
            key: key.to_string(),
        },
    )?;
    Ok(existing.rows.iter().all(|r| row_provider_is(r, principal)))
}

/// The key value as a string; `Ok(None)` only when absent. Other types are
/// rejected rather than treated as absent, which would skip the ownership check.
fn key_text(v: Option<&DbValue>) -> Result<Option<String>> {
    match v {
        None => Ok(None),
        Some(DbValue::Text(s)) => Ok(Some(s.clone())),
        Some(DbValue::Int(i)) => Ok(Some(i.to_string())),
        Some(_) => Err(anyhow!("db.op: key value must be text or integer")),
    }
}

/// Run `op` for `principal` under the namespace policy: its own `plug__`
/// namespace freely, core tables only per [`core_table_access`], nothing else.
fn exec_db_op_policed(conn: &db::Conn, op: &DbOp, principal: &str) -> Result<DbReply> {
    let table = op_table(op);
    if db::plugin_tables::validate_segment("principal", principal).is_err() {
        return Err(deny(principal, op, table, "invalid principal"));
    }
    let ns = op.namespace();
    if ns == principal {
        return db::plugin_tables::exec_db_op(conn, op);
    }
    if !ns.is_empty() {
        return Err(deny(principal, op, table, "foreign namespace"));
    }
    let Some(access) = core_table_access(table, principal) else {
        return Err(deny(principal, op, table, "core table not permitted"));
    };
    let is_read = matches!(op, DbOp::List { .. } | DbOp::Get { .. });
    match access {
        CoreTableAccess::ReadWrite => db::plugin_tables::exec_db_op(conn, op),
        CoreTableAccess::ReadOnly if is_read => db::plugin_tables::exec_db_op(conn, op),
        CoreTableAccess::ReadOnly => Err(deny(principal, op, table, "table is read-only")),
        CoreTableAccess::SharedByProvider if is_read => {
            let mut reply = db::plugin_tables::exec_db_op(conn, op)?;
            reply.rows.retain(|r| row_provider_is(r, principal));
            Ok(reply)
        }
        CoreTableAccess::SharedByProvider => {
            // IMMEDIATE so no other writer can reassign the row between the
            // ownership check and the write. Dropping `tx` (error or panic)
            // rolls back.
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let reply = exec_shared_write(&tx, op, table, principal)?;
            tx.commit()?;
            Ok(reply)
        }
    }
}

/// A write to a provider-shared table. A foreign row yields `affected: 0`,
/// indistinguishable from a missing one.
fn exec_shared_write(conn: &db::Conn, op: &DbOp, table: &str, principal: &str) -> Result<DbReply> {
    let owned = match op {
        DbOp::List { .. } | DbOp::Get { .. } => true,
        DbOp::Insert { row, .. } | DbOp::Upsert { row, .. } if !row_provider_is(row, principal) => {
            return Err(deny(
                principal,
                op,
                table,
                "row provider must be the caller",
            ));
        }
        DbOp::Insert { .. } => true,
        DbOp::Upsert { row, .. } => match key_text(row.get("id"))? {
            Some(id) => existing_rows_owned(conn, table, "id", &id, principal)?,
            None => true,
        },
        DbOp::Update { key_col, row, .. } => {
            if row.get("provider").is_some() && !row_provider_is(row, principal) {
                return Err(deny(
                    principal,
                    op,
                    table,
                    "row provider must be the caller",
                ));
            }
            let Some(key) = key_text(row.get(key_col))? else {
                return Err(deny(principal, op, table, "update without key value"));
            };
            existing_rows_owned(conn, table, key_col, &key, principal)?
        }
        DbOp::Delete { key_col, key, .. } => {
            existing_rows_owned(conn, table, key_col, key, principal)?
        }
    };
    if !owned {
        return Ok(DbReply::default());
    }
    db::plugin_tables::exec_db_op(conn, op)
}

/// Execute one capability request on behalf of `principal` — the authoritative
/// plugin id bound to this session's socket (see
/// [`supervisor::PluginProcess`](crate::supervisor::PluginProcess)). `args` is
/// the op payload the plugin sent (a serialized [`DbOp`] / [`SecretOp`] /
/// [`HttpRequest`]); the returned `Value` is the reply the supervisor wraps into
/// a `CapResult`. `db.op` / `secret.op` are confined to `principal`'s namespace.
pub fn handle_cap(cap: &str, args: Value, principal: &str) -> Result<Value> {
    match cap {
        "db.op" => {
            let op: DbOp =
                serde_json::from_value(args).map_err(|e| anyhow!("db.op: bad op payload: {e}"))?;
            let reply =
                db::pool::with_pooled_or_open(|conn| exec_db_op_policed(conn, &op, principal))?;
            Ok(serde_json::to_value(reply)?)
        }
        "secret.op" => {
            let op: SecretOp = serde_json::from_value(args)
                .map_err(|e| anyhow!("secret.op: bad op payload: {e}"))?;
            if !secret_name_owned(op.name(), principal) {
                bail!(
                    "plugin '{principal}' may not secret.op/{} outside its namespace (target '{}')",
                    op.kind(),
                    op.name()
                );
            }
            let reply = secrets::exec_secret_op_pooled(&op)?;
            Ok(serde_json::to_value(reply)?)
        }
        "http.request" => {
            let req: HttpRequest = serde_json::from_value(args)
                .map_err(|e| anyhow!("http.request: bad request payload: {e}"))?;
            let reply = exec_http(req)?;
            Ok(serde_json::to_value(reply)?)
        }
        "agents.register" => {
            let reg: AgentRegistration = serde_json::from_value(args)
                .map_err(|e| anyhow!("agents.register: bad payload: {e}"))?;
            if !agent_provider_owned(&reg.name, principal) {
                return Err(anyhow!(
                    "agents.register: plugin '{principal}' may not register provider '{}'; the provider name must be '{principal}' or '<owner>/{principal}'",
                    reg.name
                ));
            }
            agents::register_from_json(
                reg.name,
                &reg.agents_json,
                &reg.hooks_json,
                &reg.skills_json,
                &reg.commands_json,
                &reg.prompt_fragments_json,
            );
            Ok(Value::Null)
        }
        other => Err(anyhow!("unknown capability '{other}'")),
    }
}

/// Perform an [`HttpRequest`] on the daemon's single HTTP/TLS stack and return
/// the response for any status (a delegating plugin sees 4xx/5xx verbatim).
fn exec_http(req: HttpRequest) -> Result<HttpResponse> {
    let mut builder = http_client()
        .request_str(&req.method, &req.url)
        .map_err(|e| anyhow!("http.request: {e}"))?
        .insecure(req.insecure);
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    if !req.body.is_empty() {
        // The plugin's own Content-Type header (relayed above) applies; the
        // capability passes the byte body through verbatim.
        builder = builder.raw_body(req.body);
    }
    if let Some(ms) = req.timeout_ms {
        builder = builder.timeout(Duration::from_millis(ms));
    }
    let resp = cap_runtime()
        .block_on(builder.send_raw())
        .map_err(|e| anyhow!("http.request: {e}"))?;
    Ok(HttpResponse {
        status: resp.status,
        headers: resp.headers.into_iter().collect(),
        body: resp.body,
    })
}

/// Execute one STREAMING capability request, invoking `on_chunk` for each chunk
/// as it is produced. `on_chunk(seq, data)` is the supervisor's frame-writer: it
/// emits a `CapStreamChunk{ id, seq, data }`. `seq` starts at 0 (the stream
/// head) and increments per body chunk. Returns `Ok(())` on a clean end (the
/// supervisor then writes `CapStreamEnd{ ok: true }`) or `Err` on a mid-stream
/// failure (the supervisor writes `CapStreamEnd{ ok: false, error }`).
///
/// If `on_chunk` returns `Err` (the plugin aborted / the socket write failed),
/// consumption stops immediately and that error propagates.
pub fn handle_cap_stream(
    cap: &str,
    args: Value,
    on_chunk: &mut dyn FnMut(u64, Value) -> Result<()>,
) -> Result<()> {
    match cap {
        "http.stream" => {
            let req: HttpStreamRequest = serde_json::from_value(args)
                .map_err(|e| anyhow!("http.stream: bad request payload: {e}"))?;
            exec_http_stream(req, on_chunk)
        }
        other => Err(anyhow!("unknown streaming capability '{other}'")),
    }
}

/// Drive an [`HttpStreamRequest`] on the daemon's HTTP stack, relaying the
/// status and headers as `seq == 0` ([`HttpStreamChunk::Head`]) and each body
/// byte-slice as `seq >= 1` ([`HttpStreamChunk::Body`]). Never buffers the
/// whole body.
fn exec_http_stream(
    req: HttpStreamRequest,
    on_chunk: &mut dyn FnMut(u64, Value) -> Result<()>,
) -> Result<()> {
    let mut builder = http_client()
        .request_str(&req.method, &req.url)
        .map_err(|e| anyhow!("http.stream: {e}"))?
        .insecure(req.insecure);
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    if !req.body.is_empty() {
        builder = builder.raw_body(req.body);
    }
    if let Some(ms) = req.timeout_ms {
        builder = builder.timeout(Duration::from_millis(ms));
    }

    cap_runtime().block_on(async move {
        let resp = builder
            .send_stream()
            .await
            .map_err(|e| anyhow!("http.stream: {e}"))?;
        // seq 0: the head (status + headers), before any body byte.
        let head = HttpStreamChunk::Head {
            status: resp.status(),
            headers: resp
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        on_chunk(0, serde_json::to_value(head)?)?;

        let mut body = Box::pin(resp.bytes_stream());
        let mut seq = 1u64;
        while let Some(item) = plugin_toolkit::stream::next(&mut body).await {
            let bytes = item.map_err(|e| anyhow!("http.stream: body: {e}"))?;
            let chunk = HttpStreamChunk::Body { bytes };
            on_chunk(seq, serde_json::to_value(chunk)?)?;
            seq += 1;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::serde_json::json;

    #[test]
    fn unknown_capability_errors() {
        let err = handle_cap("bogus.cap", json!({}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown capability"), "got: {err}");
    }

    #[test]
    fn agents_register_refuses_foreign_provider_name() {
        let err = handle_cap(
            "agents.register",
            json!({"name": "victim-xyz", "hooks_json": "[]"}),
            "attacker-xyz",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("may not register provider 'victim-xyz'"),
            "got: {err}"
        );
        assert!(
            agents::registry::providers()
                .iter()
                .all(|p| p.name() != "victim-xyz")
        );
    }

    #[test]
    fn agent_provider_owned_accepts_id_and_owner_qualified_id() {
        assert!(agent_provider_owned("agents", "agents"));
        assert!(agent_provider_owned("argyle-labs/agents", "agents"));
        assert!(!agent_provider_owned("argyle-labs/agents", "evil"));
        assert!(!agent_provider_owned("/agents", "agents"));
        assert!(!agent_provider_owned("agents-x", "agents"));
    }

    #[test]
    fn agents_register_accepts_own_provider_name() {
        handle_cap(
            "agents.register",
            json!({"name": "self-xyz", "agents_json": r#"[{"name":"self-agent-xyz","body":"b","origin":"spoofed"}]"#}),
            "self-xyz",
        )
        .unwrap();
        let agent = agents::compose_agents()
            .into_iter()
            .find(|a| a.name == "self-agent-xyz")
            .unwrap();
        assert_eq!(agent.origin, "self-xyz");
        agents::deregister_provider("self-xyz");
    }

    #[test]
    fn http_request_rejects_malformed_payload() {
        // Missing required `url`/`method` fails at deserialization — pure
        // routing/validation, no network.
        let err = handle_cap("http.request", json!({"method": "GET"}), "p")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("http.request: bad request payload"),
            "got: {err}"
        );
    }

    #[test]
    fn malformed_op_payload_errors_before_execution() {
        // A db.op whose payload isn't a valid DbOp fails at deserialization —
        // no db needed, so this is a pure routing/validation check.
        let err = handle_cap("db.op", json!({"not": "a valid op"}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("db.op: bad op payload"), "got: {err}");
    }

    #[test]
    fn secret_name_ownership() {
        assert!(secret_name_owned("proxmox", "proxmox"));
        assert!(secret_name_owned("proxmox.pve1.token", "proxmox"));
        assert!(!secret_name_owned("jellyfin.api_key", "proxmox"));
        assert!(!secret_name_owned("anthropic_api_key", "proxmox"));
        // A prefix that isn't a namespace boundary must not be treated as owned.
        assert!(!secret_name_owned("proxmoxen.token", "proxmox"));
    }

    #[test]
    fn secret_op_refuses_foreign_name() {
        let err = handle_cap(
            "secret.op",
            serde_json::to_value(SecretOp::Get {
                name: "jellyfin.api_key".into(),
            })
            .unwrap(),
            "proxmox",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("outside its namespace"), "got: {err}");
    }

    fn policy_conn() -> db::Conn {
        let conn = db::Conn::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE endpoints (id TEXT PRIMARY KEY, provider TEXT NOT NULL, name TEXT NOT NULL);
             INSERT INTO endpoints VALUES ('a1', 'alpha', 'one'), ('b1', 'beta', 'two');
             CREATE TABLE ntfy_endpoints (name TEXT PRIMARY KEY);
             CREATE TABLE mcp_servers (name TEXT PRIMARY KEY);
             CREATE TABLE plugin_credentials (name TEXT PRIMARY KEY);",
        )
        .unwrap();
        conn
    }

    fn list(table: &str) -> DbOp {
        DbOp::List {
            namespace: String::new(),
            table: table.into(),
        }
    }

    fn count_provider(conn: &db::Conn, provider: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM endpoints WHERE provider = ?1",
            [provider],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn ep_row(id: &str, provider: &str) -> DbRow {
        let mut r = DbRow::new();
        r.insert("id".into(), DbValue::Text(id.into()));
        r.insert("provider".into(), DbValue::Text(provider.into()));
        r.insert("name".into(), DbValue::Text("n".into()));
        r
    }

    fn name_row() -> DbRow {
        let mut r = DbRow::new();
        r.insert("name".into(), DbValue::Text("x".into()));
        r
    }

    #[test]
    fn db_op_refuses_settings_read() {
        let conn = policy_conn();
        let err = exec_db_op_policed(&conn, &list("settings"), "alpha")
            .unwrap_err()
            .to_string();
        assert!(err.contains("'settings'"), "got: {err}");
    }

    #[test]
    fn db_op_refuses_secrets_write() {
        let conn = policy_conn();
        let op = DbOp::Upsert {
            namespace: String::new(),
            table: "secrets".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &op, "mcp").is_err());
    }

    #[test]
    fn endpoints_list_returns_only_own_rows() {
        let conn = policy_conn();
        let reply = exec_db_op_policed(&conn, &list("endpoints"), "alpha").unwrap();
        assert_eq!(reply.rows.len(), 1);
        assert!(row_provider_is(&reply.rows[0], "alpha"));
    }

    #[test]
    fn endpoints_update_and_delete_of_foreign_row_refused() {
        let conn = policy_conn();
        let upd = DbOp::Update {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            row: ep_row("b1", "alpha"),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &upd, "alpha").unwrap().affected,
            0
        );
        let del = DbOp::Delete {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            key: "b1".into(),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &del, "alpha").unwrap().affected,
            0
        );
        let missing = DbOp::Delete {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            key: "zz".into(),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &missing, "alpha")
                .unwrap()
                .affected,
            0
        );
        assert_eq!(count_provider(&conn, "beta"), 1);
        let own = DbOp::Update {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            row: ep_row("a1", "alpha"),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &own, "alpha").unwrap().affected,
            1
        );
    }

    #[test]
    fn endpoints_insert_with_foreign_provider_refused() {
        let conn = policy_conn();
        let op = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("a2", "beta"),
        };
        assert!(exec_db_op_policed(&conn, &op, "alpha").is_err());
        let hijack = DbOp::Upsert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("b1", "alpha"),
        };
        assert_eq!(
            exec_db_op_policed(&conn, &hijack, "alpha")
                .unwrap()
                .affected,
            0
        );
        assert_eq!(count_provider(&conn, "beta"), 1);
    }

    #[test]
    fn ntfy_endpoints_only_for_ntfy() {
        let conn = policy_conn();
        assert!(exec_db_op_policed(&conn, &list("ntfy_endpoints"), "ntfy").is_ok());
        assert!(exec_db_op_policed(&conn, &list("ntfy_endpoints"), "proxmox").is_err());
        assert!(exec_db_op_policed(&conn, &list("proxmox_endpoints"), "proxmox").is_err());
    }

    #[test]
    fn every_policy_table_exists_in_migrated_schema() {
        let conn = db::testing::test_conn();
        for (table, _, _) in CORE_TABLE_POLICY {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "policy table `{table}` has no migration");
        }
    }

    #[test]
    fn mcp_tables_only_for_mcp() {
        let conn = policy_conn();
        let ins = DbOp::Insert {
            namespace: String::new(),
            table: "mcp_servers".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &ins, "mcp").is_ok());
        assert!(exec_db_op_policed(&conn, &list("mcp_servers"), "alpha").is_err());
    }

    #[test]
    fn plugin_credentials_read_only_for_mcp() {
        let conn = policy_conn();
        assert!(exec_db_op_policed(&conn, &list("plugin_credentials"), "mcp").is_ok());
        assert!(exec_db_op_policed(&conn, &list("plugin_credentials"), "alpha").is_err());
        let ins = DbOp::Insert {
            namespace: String::new(),
            table: "plugin_credentials".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &ins, "mcp").is_err());
    }

    #[test]
    fn foreign_plugin_namespace_refused() {
        let conn = policy_conn();
        let op = DbOp::List {
            namespace: "beta".into(),
            table: "things".into(),
        };
        assert!(exec_db_op_policed(&conn, &op, "alpha").is_err());
        let via_core = list("plug__beta__things");
        assert!(exec_db_op_policed(&conn, &via_core, "alpha").is_err());
    }

    #[test]
    fn invalid_principal_refused() {
        let conn = policy_conn();
        for p in ["", "Bad", "a__b"] {
            let op = DbOp::List {
                namespace: p.into(),
                table: "things".into(),
            };
            assert!(exec_db_op_policed(&conn, &op, p).is_err(), "{p:?}");
        }
    }

    #[test]
    fn core_table_write_never_runs_ddl() {
        let conn = policy_conn();
        let mut row = ep_row("a2", "alpha");
        row.insert("evil".into(), DbValue::Text("x".into()));
        let op = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row,
        };
        let err = exec_db_op_policed(&conn, &op, "alpha")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown column"), "got: {err}");
        let cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('endpoints') WHERE name = 'evil'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cols, 0);
        // A missing core table is not created either.
        let ins = DbOp::Insert {
            namespace: String::new(),
            table: "mcp_tool_mappings".into(),
            row: name_row(),
        };
        assert!(exec_db_op_policed(&conn, &ins, "mcp").is_err());
    }

    #[test]
    fn non_text_key_rejected() {
        let conn = policy_conn();
        let mut row = ep_row("b1", "alpha");
        row.insert("id".into(), DbValue::Blob(b"b1".to_vec()));
        let ups = DbOp::Upsert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: row.clone(),
        };
        assert!(exec_db_op_policed(&conn, &ups, "alpha").is_err());
        let upd = DbOp::Update {
            namespace: String::new(),
            table: "endpoints".into(),
            key_col: "id".into(),
            row,
        };
        assert!(exec_db_op_policed(&conn, &upd, "alpha").is_err());
    }

    #[test]
    fn shared_write_leaves_no_open_transaction() {
        let conn = policy_conn();
        let bad = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("a1", "alpha"),
        };
        assert!(exec_db_op_policed(&conn, &bad, "alpha").is_err());
        assert!(conn.is_autocommit());
        let mut extra = ep_row("a4", "alpha");
        extra.insert("evil".into(), DbValue::Text("x".into()));
        let failing = DbOp::Upsert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: extra,
        };
        assert!(exec_db_op_policed(&conn, &failing, "alpha").is_err());
        assert!(conn.is_autocommit());
        assert_eq!(count_provider(&conn, "alpha"), 1);
        let ok = DbOp::Insert {
            namespace: String::new(),
            table: "endpoints".into(),
            row: ep_row("a3", "alpha"),
        };
        assert_eq!(exec_db_op_policed(&conn, &ok, "alpha").unwrap().affected, 1);
        assert!(conn.is_autocommit());
    }

    #[test]
    fn capabilities_list_is_advertised() {
        assert!(CAPABILITIES.contains(&"db.op"));
        assert!(CAPABILITIES.contains(&"secret.op"));
        assert!(CAPABILITIES.contains(&"http.request"));
    }

    #[test]
    fn only_http_stream_is_streaming() {
        assert!(is_streaming_cap("http.stream"));
        assert!(!is_streaming_cap("http.request"));
        assert!(!is_streaming_cap("db.op"));
        assert!(!is_streaming_cap("secret.op"));
        assert!(!is_streaming_cap("agents.register"));
        assert!(!is_streaming_cap("bogus"));
    }

    #[test]
    fn secret_op_rejects_malformed_payload() {
        // A secret.op whose payload isn't a valid SecretOp fails at
        // deserialization before any secret store is touched — pure routing.
        let err = handle_cap("secret.op", json!({"not": "a valid op"}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("secret.op: bad op payload"), "got: {err}");
    }

    #[test]
    fn agents_register_rejects_malformed_payload() {
        // A wrongly-typed field fails at deserialization before anything is
        // registered — pure routing/validation.
        let err = handle_cap("agents.register", json!({"name": 123}), "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("agents.register: bad payload"), "got: {err}");
    }

    #[test]
    fn stream_unknown_capability_errors() {
        let mut sink = |_seq: u64, _v: Value| Ok(());
        let err = handle_cap_stream("bogus.cap", json!({}), &mut sink)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown streaming capability"), "got: {err}");
    }

    /// Spawn a one-shot HTTP/1.1 server on an ephemeral loopback port that
    /// replies to the first connection with `response` (raw bytes, including
    /// status line + headers + body) and then closes. Returns the bound
    /// `http://127.0.0.1:PORT/` base URL. The server thread self-terminates
    /// after serving one request.
    fn oneshot_http(response: &'static [u8]) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Drain the request line/headers so the client's write completes.
                let mut buf = [0u8; 1024];
                if sock.read(&mut buf).is_ok()
                    && sock.write_all(response).is_ok()
                    && sock.flush().is_ok()
                {
                    // served
                }
            }
        });
        format!("http://{addr}/")
    }

    #[test]
    fn http_request_relays_status_headers_and_body() {
        let url =
            oneshot_http(b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\nX-Test: yes\r\n\r\nhello");
        let reply = handle_cap("http.request", json!({"method": "GET", "url": url}), "p")
            .expect("request succeeds");
        let resp: HttpResponse = serde_json::from_value(reply).expect("decode HttpResponse");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, b"hello");
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-test") && v == "yes"),
            "missing relayed header: {:?}",
            resp.headers
        );
    }

    #[test]
    fn http_request_relays_error_status_verbatim() {
        // A 4xx is returned as a normal response, not an Err — a delegating
        // plugin sees the status verbatim.
        let url = oneshot_http(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let reply = handle_cap("http.request", json!({"method": "GET", "url": url}), "p")
            .expect("request succeeds even for 4xx");
        let resp: HttpResponse = serde_json::from_value(reply).expect("decode HttpResponse");
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn http_request_bad_url_errors() {
        // A syntactically invalid URL fails when the client builds the request,
        // surfacing an "http.request:" prefixed error — no network reached.
        let err = handle_cap(
            "http.request",
            json!({"method": "GET", "url": "not a valid url"}),
            "p",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("http.request:"), "got: {err}");
    }

    #[test]
    fn http_request_connection_refused_errors() {
        // Port 1 on loopback refuses; the send fails and the error is surfaced.
        let err = handle_cap(
            "http.request",
            json!({"method": "GET", "url": "http://127.0.0.1:1/", "timeout_ms": 2000}),
            "p",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("http.request:"), "got: {err}");
    }

    #[test]
    fn http_stream_relays_head_then_body_chunks() {
        let url = oneshot_http(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-S: 1\r\n\r\nworld");
        let mut seqs: Vec<u64> = Vec::new();
        let mut status: Option<u16> = None;
        let mut body: Vec<u8> = Vec::new();
        {
            let mut sink = |seq: u64, v: Value| -> Result<()> {
                seqs.push(seq);
                let chunk: HttpStreamChunk = serde_json::from_value(v)?;
                match chunk {
                    HttpStreamChunk::Head { status: s, .. } => status = Some(s),
                    HttpStreamChunk::Body { bytes } => body.extend_from_slice(&bytes),
                }
                Ok(())
            };
            handle_cap_stream(
                "http.stream",
                json!({"method": "GET", "url": url}),
                &mut sink,
            )
            .expect("stream succeeds");
        }
        // seq 0 is always the head; body chunks follow with seq >= 1.
        assert_eq!(seqs.first().copied(), Some(0));
        assert_eq!(status, Some(200));
        assert_eq!(body, b"world");
        assert!(seqs.iter().skip(1).all(|&s| s >= 1), "seqs: {seqs:?}");
    }

    #[test]
    fn http_stream_propagates_sink_error() {
        // If the on_chunk sink returns Err (e.g. the plugin aborted), consumption
        // stops and that error propagates out of handle_cap_stream.
        let url = oneshot_http(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
        let mut sink = |_seq: u64, _v: Value| -> Result<()> { Err(anyhow!("sink boom")) };
        let err = handle_cap_stream(
            "http.stream",
            json!({"method": "GET", "url": url}),
            &mut sink,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("sink boom"), "got: {err}");
    }

    #[test]
    fn http_stream_bad_url_errors() {
        let mut sink = |_seq: u64, _v: Value| -> Result<()> { Ok(()) };
        let err = handle_cap_stream(
            "http.stream",
            json!({"method": "GET", "url": "not a valid url"}),
            &mut sink,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("http.stream:"), "got: {err}");
    }

    #[test]
    fn stream_rejects_malformed_payload() {
        // http.stream with a payload missing method/url fails at deserialization
        // before any network I/O — the on_chunk sink is never invoked.
        let mut called = false;
        let mut sink = |_seq: u64, _v: Value| {
            called = true;
            Ok(())
        };
        let err = handle_cap_stream("http.stream", json!({"method": "GET"}), &mut sink)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("http.stream: bad request payload"),
            "got: {err}"
        );
        assert!(!called, "sink must not fire on a payload error");
    }
}
