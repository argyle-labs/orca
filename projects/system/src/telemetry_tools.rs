//! `system.telemetry.list` — per-host snapshot timeseries.
//!
//! The recorded `host_status` snapshots for a host, newest-first (the UI's
//! sparkline/timeseries source). This is the reframed home of the former
//! `system.topology view=history` — a distinct dataset from `system.history` (which
//! is the `db::metrics` series). Named for what it is: periodic host telemetry
//! snapshots, not "history".
//!
//! Storage holds only this host's own rows (telemetry is local-only, fetched on
//! demand), so `--id <other system>` is forwarded to that system, which reads
//! and stamps its own rows. Rows are only ever stamped with the id of the
//! system that read them. Read-only — writers live in the server's background
//! tasks.
//!
//! Lives in the `system` crate (not `mesh`): it reads only `hosts::host_status`,
//! `db::metrics`, and this crate's `SystemInfoReport`, so it carries no mesh
//! dependency — part of dissolving `system.topology`.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::system_id::SystemId;
use crate::system_info_types::SystemInfoReport;

#[derive::snake_aliases]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySnapshotRow {
    /// The id of the system that recorded this snapshot.
    pub system_id: String,
    pub snapshot_at_unix: i64,
    pub received_at_unix: i64,
    /// Always `"local"`: every row is read from the recording system's storage.
    pub source: String,
    /// Decoded snapshot. Absent if the stored payload couldn't be parsed
    /// (typically: a schema mismatch after an upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfoReport>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TelemetrySnapshots(pub Vec<TelemetrySnapshotRow>);

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemTelemetryListArgs {
    /// Refused with a 400 unless null. These args carry no
    /// `deny_unknown_fields`, so an ignored `peerId` would answer with this
    /// system's rows; another system's telemetry is read with `id`. Null is
    /// accepted because clients that serialize an unset optional `peerId`
    /// send it.
    #[serde(
        default,
        alias = "peer_id",
        deserialize_with = "refuse_peer_id",
        skip_serializing
    )]
    #[schemars(skip)]
    #[arg(skip)]
    pub peer_id: Option<()>,
    /// Read ONE system's telemetry, by its id (the stable `machineId` UUID).
    /// Omit for this system.
    #[arg(long)]
    pub id: Option<SystemId>,
    /// Return only rows with `snapshot_at_unix > since_unix`. Omit for the
    /// full retained history (capped in storage).
    #[arg(long)]
    pub since_unix: Option<i64>,
    /// Maximum rows to return. Defaults to 256 — a day at 1/min with room to
    /// spare; pass lower for sparkline-style queries.
    #[arg(long)]
    pub limit: Option<u32>,
}

fn refuse_peer_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<()>, D::Error> {
    match Option::<serde::de::IgnoredAny>::deserialize(d)? {
        None => Ok(None),
        Some(_) => Err(serde::de::Error::custom(
            "`peerId` is not an argument of system.telemetry.list: it reads the telemetry of \
             the system named by `id`; pass that system's id as `id`",
        )),
    }
}

/// This system's id, stamped onto every row it reads.
fn this_system_id() -> anyhow::Result<&'static str> {
    crate::host_identity::try_machine_id().ok_or_else(|| {
        contract::OrcaError::unavailable(
            "this system has no id: host identity was not initialized in this process",
        )
        .into()
    })
}

/// Storage rows carry no system id or source (they are always this system's
/// own), so both are stamped here.
fn rows_to_dtos(
    rows: Vec<hosts::host_status::HostStatusRow>,
    system_id: &str,
) -> Vec<TelemetrySnapshotRow> {
    rows.into_iter()
        .map(|r| {
            let system = serde_json::from_str::<SystemInfoReport>(&r.payload_json).ok();
            TelemetrySnapshotRow {
                system_id: system_id.to_string(),
                snapshot_at_unix: r.snapshot_at_unix,
                received_at_unix: r.received_at_unix,
                source: "local".to_string(),
                system,
            }
        })
        .collect()
}

/// Refuse a forwarded answer containing any row stamped with a different
/// system's id, so one host's numbers can never be returned as another's.
fn only_rows_of(id: &str, out: TelemetrySnapshots) -> anyhow::Result<TelemetrySnapshots> {
    if let Some(r) = out.0.iter().find(|r| !r.system_id.eq_ignore_ascii_case(id)) {
        anyhow::bail!(
            "system `{id}` answered with telemetry stamped `{}`; refusing to label it `{id}`",
            r.system_id
        );
    }
    Ok(out)
}

/// Per-host snapshot timeseries, newest-first. Latest snapshot is already on
/// `system.list` (each member row enriches its `system` field from the same
/// `host_status` table), so this verb is the timeseries tail behind it.
#[orca_tool(domain = "system.telemetry", verb = "list")]
async fn system_telemetry_list(
    args: SystemTelemetryListArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<TelemetrySnapshots> {
    // Without a host identity this system cannot be the one `id` names, so
    // the read still forwards rather than failing on the missing identity.
    if let Some(id) = args.id.clone()
        && !crate::host_identity::try_machine_id().is_some_and(|me| id.eq_ignore_ascii_case(me))
    {
        // Ask with NO id: the target reports on itself, so a hop can never
        // bounce onward.
        let remote = SystemTelemetryListArgs { id: None, ..args };
        let out = dispatch::cli::exec_remote::<SystemTelemetryList>(&id, remote, ctx)
            .await
            .map_err(|e| anyhow::anyhow!("telemetry of system `{id}` is unavailable: {e:#}"))?;
        return only_rows_of(&id, out);
    }
    let system_id = this_system_id()?;
    let limit = args.limit.unwrap_or(256) as usize;
    let rows = db::metrics::with_conn(|conn| {
        hosts::host_status::rows_since(conn, args.since_unix, limit)
    })?;
    Ok(TelemetrySnapshots(rows_to_dtos(rows, system_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_status_row_persisted_in_snake_case_still_decodes() {
        // `rows_to_dtos` decodes with `.ok()`, so a row written before the
        // camelCase rename that failed to decode would silently show no system.
        let rows = vec![hosts::host_status::HostStatusRow {
            snapshot_at_unix: 1,
            received_at_unix: 2,
            payload_json: r#"{"os_name":"Debian","os_version":"12","kernel_version":"6.1",
                "dmi_vendor":"Dell","mem_total_mb":4096,"cpu_model":"Xeon"}"#
                .to_string(),
        }];
        let dto = rows_to_dtos(rows, "p").pop().unwrap();
        let sys = dto.system.expect("snake row decodes");
        assert_eq!(sys.os_name.as_deref(), Some("Debian"));
        assert_eq!(sys.kernel_version.as_deref(), Some("6.1"));
        assert_eq!(sys.dmi_vendor.as_deref(), Some("Dell"));
        assert_eq!(sys.cpu_model.as_deref(), Some("Xeon"));
    }

    fn now() -> i64 {
        utils::time::now().unix_seconds()
    }

    fn metrics_conn() -> db::Conn {
        let conn = db::Conn::open_in_memory().expect("open_in_memory");
        db::metrics::init_schema(&conn).expect("init metrics schema");
        conn
    }

    /// This host's own rows, one payload malformed to exercise the `system =
    /// None` branch. Recent timestamps so age-based pruning doesn't evict them.
    fn seed(conn: &db::Conn, t: i64) {
        hosts::host_status::insert_status(conn, t - 200, "not json at all", t, 86_400).unwrap();
        hosts::host_status::insert_status(conn, t - 100, "not json at all", t, 86_400).unwrap();
    }

    /// This system's id, after initializing host identity in a throwaway dir.
    fn me() -> &'static str {
        static HOME: std::sync::LazyLock<tempfile::TempDir> =
            std::sync::LazyLock::new(|| tempfile::tempdir().expect("tempdir"));
        crate::host_identity::init(HOME.path()).expect("init host identity");
        this_system_id().expect("this system's id")
    }

    fn detail(conn: &db::Conn, since_unix: Option<i64>, limit: usize) -> Vec<TelemetrySnapshotRow> {
        let rows = hosts::host_status::rows_since(conn, since_unix, limit).unwrap();
        rows_to_dtos(rows, me())
    }

    #[test]
    fn telemetry_returns_snapshots_newest_first() {
        let conn = metrics_conn();
        let t = now();
        seed(&conn, t);
        let out = detail(&conn, None, 256);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].snapshot_at_unix, t - 100);
        assert_eq!(out[1].snapshot_at_unix, t - 200);
        assert!(out[0].system.is_none(), "unparseable payload → None");
        assert_eq!(out[0].source, "local");
    }

    #[test]
    fn rows_carry_this_systems_id() {
        let conn = metrics_conn();
        seed(&conn, now());
        let me = me();
        assert_eq!(me, crate::host_identity::machine_id());
        let out = detail(&conn, None, 256);
        assert!(!out.is_empty());
        assert!(out.iter().all(|r| r.system_id == me), "{me}");
    }

    #[test]
    fn a_peer_id_is_refused_naming_it() {
        for key in ["peerId", "peer_id"] {
            let args = serde_json::json!({ key: "019f9f7b-3333-7e40-9e30-4987d8d12dcb" });
            let err = serde_json::from_value::<SystemTelemetryListArgs>(args)
                .err()
                .unwrap_or_else(|| panic!("{key} must be refused"));
            assert!(
                err.to_string().contains("`peerId` is not an argument"),
                "{err}"
            );
        }
        let args: SystemTelemetryListArgs =
            serde_json::from_value(serde_json::json!({ "limit": 5 })).unwrap();
        assert!(args.peer_id.is_none());
    }

    #[test]
    fn a_null_peer_id_is_accepted() {
        for key in ["peerId", "peer_id"] {
            let args: SystemTelemetryListArgs =
                serde_json::from_value(serde_json::json!({ key: null, "limit": 5 }))
                    .unwrap_or_else(|e| panic!("{key}: null must be accepted: {e}"));
            assert!(args.peer_id.is_none());
            assert_eq!(args.limit, Some(5));
        }
    }

    #[test]
    fn the_schema_offers_no_address() {
        let schema = schemars::schema_for!(SystemTelemetryListArgs);
        let props = schema.get("properties").expect("properties");
        assert!(props.get("peerId").is_none(), "{props}");
    }

    const THOR: &str = "019f9f7b-3333-7e40-9e30-4987d8d12dcb";

    /// `(peer, tool, forwarded id, forwarded limit)` per forwarded call.
    type Calls =
        std::sync::Arc<std::sync::Mutex<Vec<(String, String, Option<String>, Option<u64>)>>>;

    /// Records every forwarded call and answers with one row stamped `stamp`,
    /// or fails when `stamp` is `None` (an unreachable system).
    struct SpyMesh {
        stamp: Option<&'static str>,
        calls: Calls,
    }

    #[async_trait::async_trait]
    impl contract::RemoteExec for SpyMesh {
        #[allow(clippy::disallowed_types)]
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: serde_json::Value,
            _caller: Option<contract::CallerIdentity>,
            _correlation_id: Option<String>,
        ) -> anyhow::Result<serde_json::Value> {
            self.calls.lock().unwrap().push((
                peer.to_string(),
                tool.to_string(),
                args.get("id").and_then(|v| v.as_str()).map(str::to_string),
                args.get("limit").and_then(|v| v.as_u64()),
            ));
            let Some(stamp) = self.stamp else {
                anyhow::bail!("unreachable");
            };
            Ok(serde_json::json!([{
                "systemId": stamp,
                "snapshotAtUnix": 7,
                "receivedAtUnix": 8,
                "source": "local",
            }]))
        }
    }

    fn spy_ctx(stamp: Option<&'static str>) -> (contract::ToolCtx, Calls) {
        use contract::config::{Config, Model};
        let dir = std::env::temp_dir().join("orca-telemetry-route-test");
        let mut ctx = contract::ToolCtx::new(std::sync::Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("telemetry-test.db"),
            ports: Default::default(),
        }));
        let calls = std::sync::Arc::default();
        ctx.register_service(std::sync::Arc::new(SpyMesh {
            stamp,
            calls: std::sync::Arc::clone(&calls),
        }) as std::sync::Arc<dyn contract::RemoteExec>);
        (ctx, calls)
    }

    fn addressed_to(id: &str) -> SystemTelemetryListArgs {
        serde_json::from_value(serde_json::json!({ "id": id, "limit": 5 })).unwrap()
    }

    #[tokio::test]
    async fn an_id_naming_another_system_reads_that_systems_telemetry() {
        me();
        let (ctx, calls) = spy_ctx(Some(THOR));
        let out = system_telemetry_list(addressed_to(THOR), &ctx)
            .await
            .expect("routed read");
        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].system_id, THOR);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].0, THOR);
        assert_eq!(calls[0].1, "system.telemetry.list");
        // The target reports on itself: the id is not forwarded, the filters are.
        assert_eq!(calls[0].2, None);
        assert_eq!(calls[0].3, Some(5));
    }

    #[tokio::test]
    async fn a_remote_read_forwards_without_a_host_identity() {
        // Deliberately no `me()`: under nextest this process has no identity.
        let (ctx, calls) = spy_ctx(Some(THOR));
        let out = system_telemetry_list(addressed_to(THOR), &ctx)
            .await
            .expect("forwarded without a local identity");
        assert_eq!(out.0[0].system_id, THOR);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_system_is_an_error_not_local_rows() {
        me();
        let (ctx, _) = spy_ctx(None);
        let err = system_telemetry_list(addressed_to(THOR), &ctx)
            .await
            .err()
            .expect("unreachable target must fail");
        assert!(err.to_string().contains(THOR), "{err}");
    }

    #[tokio::test]
    async fn rows_stamped_with_another_system_are_refused() {
        let me = me();
        let (ctx, _) = spy_ctx(Some(me));
        let err = system_telemetry_list(addressed_to(THOR), &ctx)
            .await
            .err()
            .expect("mislabelled rows must be refused");
        assert!(err.to_string().contains("refusing"), "{err}");
    }

    #[test]
    fn telemetry_honors_since_unix_watermark() {
        let conn = metrics_conn();
        let t = now();
        seed(&conn, t);
        let out = detail(&conn, Some(t - 150), 256);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].snapshot_at_unix, t - 100);
    }

    #[test]
    fn telemetry_honors_limit() {
        let conn = metrics_conn();
        let t = now();
        seed(&conn, t);
        let out = detail(&conn, None, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].snapshot_at_unix, t - 100);
    }

    #[test]
    fn telemetry_empty_when_no_rows() {
        let conn = metrics_conn();
        let out = detail(&conn, None, 256);
        assert_eq!(out.len(), 0);
    }
}
