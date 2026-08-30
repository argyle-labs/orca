//! Per-host **mount placements** — "host X mounts share Y at target Z" —
//! replicated pod-wide so the whole fleet's mount topology is visible and any
//! node can author a placement for any host ([[mesh-data-is-eventually-consistent]]).
//! Each host's convergence loop materializes only the rows whose `host` is its
//! own peer id.
//!
//! Supersedes the per-host-local `managed_mounts` table (which, being local and
//! unreplicated, is exactly why the fleet drifted). Named `mount` while the two
//! coexist; `managed_mounts` + autofs are retired once the convergence loop owns
//! materialization.
//!
//! A placement holds NO copy of the share's sources/options — it references the
//! share by `share_id`, so a host cannot drift a share it only points at.
//!
//! Identity: the primary key is a minted uuidv7 `id` ([[pure-uuidv7-ids-not-composite]]);
//! `name` is a human label that is unique **per host** (`UNIQUE(host, name)`),
//! not fleet-wide — `data` / `backups` / `downloads` on each host. Mesh sync is
//! last-write-wins on `updated_at`, keyed by `id`, so the row converges
//! fleet-wide instead of drifting per-host.
//!
//! Hand-written (not `#[endpoint_resource]`): the generic macro hardcodes a
//! `name` primary key and keys CRUD + replication by `name`, neither of which
//! expresses an `id` PK with a per-host `name` label. The mount tool surface is
//! hand-written next to `storage.mount.update` in `storage_tools.rs`.

use plugin_toolkit::storage::{Health, RemountPolicy};

/// Table name — the pod-replicated mount-placement store.
pub const TABLE: &str = "mounts";

/// Columns carried over mesh replication — CONFIG only, in CREATE TABLE order
/// (PK first, `updated_at` LWW clock last). The per-tick runtime columns
/// `health` and `active_route` are deliberately absent: they are host-LOCAL
/// (each host's convergence loop measures its own), so replicating them would
/// let a peer's echo clobber this host's freshly-probed liveness
/// ([[data-classification-config-syncs-history-local]]). They live in
/// [`LOCAL_ONLY_COLS`] and are preserved across a merge instead of overwritten.
const REPLICATED_COLS: &[&str] = &[
    "id",
    "name",
    "share_id",
    "host",
    "target",
    "guest",
    "remount_policy",
    "enabled",
    "created_at",
    "updated_at",
];

/// Host-LOCAL runtime columns — never exported, never merged. `merge_table_natural`
/// reads them off the local row before its DELETE+INSERT and re-applies them, so a
/// peer's merge updates config without disturbing this host's liveness.
/// `active_options` (the comma-joined live `-o` option tokens the kernel reports)
/// and `drift` (whether those diverge from the share's rendered options) are
/// per-host measurements too — a peer must never echo them back over this host's
/// freshly-observed reality — so they join `health`/`active_route` here.
/// `multi_mounted` (the tick observed >1 mount stacked at this target) is the same
/// kind of per-host reality and joins them.
const LOCAL_ONLY_COLS: &[&str] = &[
    "health",
    "active_route",
    "active_options",
    "drift",
    "multi_mounted",
];

/// Own-table schema. `id` is the uuidv7 PK; `UNIQUE(host, name)` makes `name` a
/// per-host label. `updated_at` is the macro-style unix-millis LWW clock
/// ([[time-values-in-milliseconds]]) stamped on every write.
const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS mounts (\n    \
    id TEXT PRIMARY KEY,\n    \
    name TEXT NOT NULL,\n    \
    share_id TEXT NOT NULL,\n    \
    host TEXT NOT NULL,\n    \
    target TEXT NOT NULL,\n    \
    guest TEXT,\n    \
    remount_policy TEXT,\n    \
    health TEXT NOT NULL DEFAULT 'missing',\n    \
    active_route TEXT,\n    \
    active_options TEXT,\n    \
    drift INTEGER NOT NULL DEFAULT 0,\n    \
    multi_mounted INTEGER NOT NULL DEFAULT 0,\n    \
    enabled INTEGER NOT NULL DEFAULT 1,\n    \
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),\n    \
    updated_at INTEGER NOT NULL DEFAULT 0,\n    \
    UNIQUE(host, name)\n);";

plugin_toolkit::inventory::submit! {
    plugin_toolkit::SchemaFragment { name: TABLE, sql: CREATE_TABLE_SQL }
}

// ── Mesh replication (last-write-wins on `updated_at`, keyed by `id`) ──────────
// Free-form JSON at the replication boundary is intentional (heterogeneous
// per-table rows) — same justified allow the generic replicator carries.
#[allow(clippy::disallowed_types)]
fn replicate_export_mounts(
    conn: &plugin_toolkit::rusqlite::Connection,
) -> plugin_toolkit::anyhow::Result<plugin_toolkit::serde_json::Value> {
    plugin_toolkit::replicate_table::export_table(conn, TABLE, REPLICATED_COLS, "id")
}

#[allow(clippy::disallowed_types)]
fn replicate_merge_mounts(
    conn: &plugin_toolkit::rusqlite::Connection,
    rows: plugin_toolkit::serde_json::Value,
) -> plugin_toolkit::anyhow::Result<usize> {
    plugin_toolkit::replicate_table::merge_table_natural(
        conn,
        TABLE,
        REPLICATED_COLS,
        "id",
        &["host", "name"],
        "updated_at",
        LOCAL_ONLY_COLS,
        rows,
    )
}

plugin_toolkit::inventory::submit! {
    plugin_toolkit::ReplicatedRegistration {
        name: TABLE,
        export: replicate_export_mounts,
        merge: replicate_merge_mounts,
    }
}

/// A desired mount placement row.
#[derive(Debug, Clone)]
pub struct EndpointRow {
    /// Minted uuidv7 identity — the primary key.
    pub id: String,
    /// Human label, unique per `host` (`data` / `backups` / `downloads`).
    pub name: String,
    /// The share this placement mounts, by its uuidv7 `shares.id`.
    pub share_id: String,
    /// The peer id of the host this placement targets. A host's convergence loop
    /// acts only on rows whose `host` equals its own peer id.
    pub host: String,
    /// Absolute mountpoint on `host`.
    pub target: String,
    /// Typed remount policy (per-placement host behaviour). `None` ⇒ the engine
    /// applies [`RemountPolicy::default`].
    pub remount_policy: Option<RemountPolicy>,
    /// Last-known liveness, written by the convergence tick each pass. Never
    /// probed live in a read verb — `storage.mount.detail` returns this stored
    /// value so the read path stays within budget.
    pub health: Health,
    /// The source (`host:/export`) the convergence tick last mounted this
    /// placement from, when known. `None` before the first successful mount.
    pub active_route: Option<String>,
    /// Comma-joined live `-o` option tokens the kernel reports for this mount,
    /// written by the convergence tick each pass. `None` when nothing is mounted
    /// at the target. Host-LOCAL runtime state — never replicated.
    pub active_options: Option<String>,
    /// Whether the live mount options diverge from the share's rendered options
    /// (an operator changed e.g. `hard`→`soft` and this host has not yet
    /// remounted). Host-LOCAL runtime state — never replicated.
    pub drift: bool,
    /// Whether the convergence tick observed more than one mount stacked at this
    /// placement's target (an anomaly the write path blocks but a reconcile must
    /// still tolerate + surface). Host-LOCAL runtime state — never replicated.
    pub multi_mounted: bool,
    /// Whether this placement is materialized by the convergence loop.
    pub enabled: bool,
    /// When set, the placement is applied INSIDE this guest (an LXC vmid or VM
    /// name) on `host` — the host's convergence loop hands it to a
    /// [`GuestMountApplier`] (e.g. the proxmox plugin renders an `lxc.mount.entry`
    /// so an unprivileged guest gets the share's mount, lifecycle-tied to the
    /// guest) instead of mounting it on the host filesystem. `None` ⇒ an ordinary
    /// host mount at `target`. Replicated config (any node may author it).
    pub guest: Option<String>,
}

/// Hand-written DB layer, keyed by the uuidv7 `id`. Every op runs through core's
/// single pooled connection via `runtime::db_op` (typed [`DbOp`]) — the plugin
/// never opens its own connection.
pub mod endpoint_db {
    use super::{EndpointRow, TABLE};
    use plugin_toolkit::abi::{DbOp, DbRow, DbValue};
    use plugin_toolkit::anyhow::Result;
    use plugin_toolkit::runtime::{ToDbValue, db_op, field_from_row};

    /// Build the DB row, stamping the `updated_at` LWW clock. `created_at` is
    /// omitted so its column default applies on insert and it is left untouched
    /// on update.
    fn to_dbrow(ep: &EndpointRow) -> DbRow {
        let mut m = DbRow::new();
        m.insert("id".to_string(), DbValue::Text(ep.id.clone()));
        m.insert("name".to_string(), DbValue::Text(ep.name.clone()));
        m.insert("share_id".to_string(), DbValue::Text(ep.share_id.clone()));
        m.insert("host".to_string(), DbValue::Text(ep.host.clone()));
        m.insert("target".to_string(), DbValue::Text(ep.target.clone()));
        m.insert("guest".to_string(), ToDbValue::to_dbvalue(&ep.guest));
        m.insert(
            "remount_policy".to_string(),
            match &ep.remount_policy {
                Some(p) => DbValue::Text(
                    plugin_toolkit::serde_json::to_string(p).unwrap_or_else(|_| "{}".to_string()),
                ),
                None => DbValue::Null,
            },
        );
        m.insert(
            "health".to_string(),
            DbValue::Text(
                plugin_toolkit::serde_json::to_value(ep.health)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "missing".to_string()),
            ),
        );
        m.insert(
            "active_route".to_string(),
            ToDbValue::to_dbvalue(&ep.active_route),
        );
        m.insert(
            "active_options".to_string(),
            ToDbValue::to_dbvalue(&ep.active_options),
        );
        m.insert("drift".to_string(), DbValue::Bool(ep.drift));
        m.insert("multi_mounted".to_string(), DbValue::Bool(ep.multi_mounted));
        m.insert("enabled".to_string(), DbValue::Bool(ep.enabled));
        m.insert(
            "updated_at".to_string(),
            DbValue::Int(plugin_toolkit::now_millis_since_epoch()),
        );
        m
    }

    fn from_dbrow(m: &DbRow) -> Result<EndpointRow> {
        Ok(EndpointRow {
            id: field_from_row(m, "id")?,
            name: field_from_row(m, "name")?,
            share_id: field_from_row(m, "share_id")?,
            host: field_from_row(m, "host")?,
            target: field_from_row(m, "target")?,
            guest: field_from_row(m, "guest")?,
            remount_policy: {
                let raw: Option<String> = field_from_row(m, "remount_policy")?;
                raw.as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .map(
                        plugin_toolkit::serde_json::from_str::<
                            plugin_toolkit::storage::RemountPolicy,
                        >,
                    )
                    .transpose()
                    .unwrap_or(None)
            },
            health: {
                let raw: String = field_from_row(m, "health")?;
                plugin_toolkit::serde_json::from_value(plugin_toolkit::serde_json::Value::String(
                    raw,
                ))
                .unwrap_or(plugin_toolkit::storage::Health::Missing)
            },
            active_route: field_from_row(m, "active_route")?,
            active_options: field_from_row(m, "active_options")?,
            drift: field_from_row::<bool>(m, "drift")?,
            multi_mounted: field_from_row::<bool>(m, "multi_mounted")?,
            enabled: field_from_row::<bool>(m, "enabled")?,
        })
    }

    pub fn list() -> Result<Vec<EndpointRow>> {
        let reply = db_op(&DbOp::List {
            namespace: String::new(),
            table: TABLE.to_string(),
        })?;
        reply.rows.iter().map(from_dbrow).collect()
    }

    pub fn get_by_id(id: &str) -> Result<Option<EndpointRow>> {
        let reply = db_op(&DbOp::Get {
            namespace: String::new(),
            table: TABLE.to_string(),
            key_col: "id".to_string(),
            key: id.to_string(),
        })?;
        match reply.rows.first() {
            Some(r) => Ok(Some(from_dbrow(r)?)),
            None => Ok(None),
        }
    }

    /// Resolve the placement for a `(host, name)` pair — the per-host label is
    /// unique, so at most one row matches. SQLite has no composite `Get`, so the
    /// list is scanned client-side.
    pub fn get_by_host_name(host: &str, name: &str) -> Result<Option<EndpointRow>> {
        Ok(list()?
            .into_iter()
            .find(|m| m.host == host && m.name == name))
    }

    pub fn insert(ep: &EndpointRow) -> Result<()> {
        db_op(&DbOp::Insert {
            namespace: String::new(),
            table: TABLE.to_string(),
            row: to_dbrow(ep),
        })?;
        Ok(())
    }

    pub fn update(ep: &EndpointRow) -> Result<bool> {
        let reply = db_op(&DbOp::Update {
            namespace: String::new(),
            table: TABLE.to_string(),
            key_col: "id".to_string(),
            row: to_dbrow(ep),
        })?;
        Ok(reply.affected > 0)
    }

    pub fn remove(id: &str) -> Result<bool> {
        let reply = db_op(&DbOp::Delete {
            namespace: String::new(),
            table: TABLE.to_string(),
            key_col: "id".to_string(),
            key: id.to_string(),
        })?;
        Ok(reply.affected > 0)
    }
}
