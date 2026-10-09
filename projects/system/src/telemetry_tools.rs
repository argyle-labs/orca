//! `system.telemetry.list` — per-host snapshot timeseries.
//!
//! The recorded `host_status` snapshots for a host, newest-first (the UI's
//! sparkline/timeseries source). This is the reframed home of the former
//! `system.topology view=history` — a distinct dataset from `system.history` (which
//! is the `db::metrics` series). Named for what it is: periodic host telemetry
//! snapshots, not "history".
//!
//! Storage holds only this host's own rows (telemetry is local-only, fetched on
//! demand), so the verb takes no address: a call addressed to another system's
//! id is routed there like any other verb, and the rows read here are always
//! this system's own, stamped with its id at read time. Read-only — writers
//! live in the server's background tasks.
//!
//! Lives in the `system` crate (not `mesh`): it reads only `hosts::host_status`,
//! `db::metrics`, and this crate's `SystemInfoReport`, so it carries no mesh
//! dependency — part of dissolving `system.topology`.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    /// system's rows; another system's telemetry is read by addressing the call
    /// to its id. Null is accepted because clients that serialize an unset
    /// optional `peerId` send it.
    #[serde(
        default,
        alias = "peer_id",
        deserialize_with = "refuse_peer_id",
        skip_serializing
    )]
    #[schemars(skip)]
    #[arg(skip)]
    pub peer_id: Option<()>,
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
             the system the call is addressed to; address the call to that system's id",
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

/// Per-host snapshot timeseries, newest-first. Latest snapshot is already on
/// `system.list` (each member row enriches its `system` field from the same
/// `host_status` table), so this verb is the timeseries tail behind it.
#[orca_tool(domain = "system.telemetry", verb = "list")]
async fn system_telemetry_list(
    args: SystemTelemetryListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TelemetrySnapshots> {
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
