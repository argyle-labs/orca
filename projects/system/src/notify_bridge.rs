//! Diagnostics → dismissable-notification bridge.
//!
//! The first internal consumer of the stateful notification plane
//! (`db::notifications_store`). It runs the diagnostics fan-out
//! (`contract::diagnostics::diagnose`) and reconciles the result into
//! dismissable notifications:
//!
//! * Every `Warn`+ [`Finding`] is [`raise`](db::notifications_store::raise)d
//!   under the stable key `diag:<provider>:<finding_id>`. Re-running is
//!   idempotent (upsert); a finding the user suppressed stays suppressed.
//! * A finding's [`RepairSpec`] becomes the notification's `fix` link — either
//!   an in-place repair (`provider` + `repair_id`, run via `diagnostics.repair`)
//!   or a delegated one (the target `unit` + `action`).
//! * `actionable` = the finding carries a repair.
//! * A previously-raised diagnostics notification whose finding has **cleared**
//!   (no longer in the current fan-out) is auto-dismissed — but only if it is
//!   still `active` (a user `dismissed`/`suppressed` row is left alone).
//!
//! Audience follows the core policy (`db::notifications_store::derive_audience`):
//! a non-actionable warning stays system-side; an error/critical or any
//! actionable finding reaches the user.

use anyhow::Result;
use contract::diagnostics::{self, Finding, RepairSpec, Severity as DiagSeverity};
use db::notifications_store::{self as store, Fix, RaiseInput, Severity as NotifySeverity, State};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Prefix on `source` for notifications this bridge owns. Used to scope the
/// auto-dismiss sweep so we never clear notifications from other sources.
const SOURCE_PREFIX: &str = "diagnostics:";

fn now_ms() -> i64 {
    utils::time::now().unix_millis()
}

/// Map a diagnostics severity onto the notification severity ladder. `Ok`/`Info`
/// return `None` — they are healthy/advisory and do not raise a notification.
fn map_severity(s: DiagSeverity) -> Option<NotifySeverity> {
    match s {
        DiagSeverity::Ok | DiagSeverity::Info => None,
        DiagSeverity::Warn => Some(NotifySeverity::Warn),
        DiagSeverity::Crit => Some(NotifySeverity::Critical),
    }
}

/// Stable dedup key for a finding's notification.
fn finding_key(f: &Finding) -> String {
    format!("diag:{}:{}", f.provider, f.id)
}

/// Build the `fix` link from a finding's repair. A delegated repair records the
/// target unit + action; an in-place repair records the diagnosing provider +
/// repair id so the user can invoke `diagnostics.repair`.
fn fix_from_repair(provider: &str, repair: &RepairSpec) -> Fix {
    match &repair.delegate {
        Some(d) => Fix {
            // A readable coordinate for the delegated unit (not an identity —
            // the deep link resolves it through the unit registry).
            unit: Some(format!("{}/{}/{}", d.unit.manager, d.unit.kind, d.unit.id)),
            action: Some(d.action.clone()),
            provider: Some(provider.to_string()),
            repair_id: Some(repair.id.clone()),
            url: None,
        },
        None => Fix {
            provider: Some(provider.to_string()),
            repair_id: Some(repair.id.clone()),
            ..Default::default()
        },
    }
}

/// Turn one finding into a `RaiseInput`, or `None` if it should not raise
/// (healthy/advisory severity).
fn raise_input_for(f: &Finding) -> Option<RaiseInput> {
    let severity = map_severity(f.severity)?;
    let actionable = f.repair.is_some();
    let fix = f.repair.as_ref().map(|r| fix_from_repair(&f.provider, r));
    Some(RaiseInput {
        key: finding_key(f),
        source: format!("{SOURCE_PREFIX}{}", f.provider),
        source_ref: Some(f.id.clone()),
        severity,
        actionable,
        fix,
        title: f.title.clone(),
        body: Some(f.detail.clone()),
        user_id: None,
    })
}

/// Outcome of a bridge reconcile pass.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeReport {
    /// Keys of notifications raised (created or reactivated) this pass.
    pub raised: Vec<String>,
    /// Keys auto-dismissed because their finding cleared.
    pub cleared: Vec<String>,
}

/// Run one diagnose→notify reconcile pass. Idempotent: safe to call on a timer
/// or on demand. See the module docs for the reconcile rules.
pub async fn reconcile_diagnostics() -> Result<BridgeReport> {
    let findings = diagnostics::diagnose(diagnostics::DiagnoseArgs::default()).await;
    reconcile_with(findings)
}

/// The pure reconcile step over an already-collected finding set — split out so
/// tests can drive it without the process-global provider registry.
fn reconcile_with(findings: Vec<Finding>) -> Result<BridgeReport> {
    let now = now_ms();
    let mut report = BridgeReport::default();
    let mut current: HashSet<String> = HashSet::new();

    for f in &findings {
        if let Some(input) = raise_input_for(f) {
            current.insert(input.key.clone());
            let key = input.key.clone();
            db::pool::with_pooled_or_open(|conn| store::raise(conn, input.clone(), now))?;
            report.raised.push(key);
        }
    }

    // Auto-dismiss diagnostics notifications whose finding cleared. Only touch
    // still-active rows owned by this bridge — a user-dismissed or suppressed
    // row must stay as the user left it.
    let stale: Vec<String> = db::pool::with_pooled_or_open(|conn| {
        let active = store::list(
            conn,
            &store::ListFilter {
                state: Some(State::Active),
                audience: None,
            },
        )?;
        Ok(active
            .into_iter()
            .filter(|n| n.source.starts_with(SOURCE_PREFIX) && !current.contains(&n.key))
            .map(|n| n.key)
            .collect())
    })?;

    for key in stale {
        db::pool::with_pooled_or_open(|conn| store::dismiss(conn, &key, now))?;
        report.cleared.push(key);
    }

    report.raised.sort();
    report.cleared.sort();
    Ok(report)
}

// ── tool ─────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifySyncDiagnosticsArgs {}

/// Run the diagnostics→notification reconcile: raise a dismissable notification
/// for every `Warn`+ finding and auto-dismiss ones whose finding cleared.
/// Returns the keys raised and cleared.
#[orca_tool(domain = "notify", verb = "sync_diagnostics")]
async fn notify_sync_diagnostics(
    _args: NotifySyncDiagnosticsArgs,
    _ctx: &contract::ToolCtx,
) -> Result<BridgeReport> {
    reconcile_diagnostics().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::diagnostics::{DelegatedRepair, RepairSpec};
    use contract::unit::UnitId;

    fn finding(id: &str, provider: &str, sev: DiagSeverity, repair: Option<RepairSpec>) -> Finding {
        Finding {
            id: id.into(),
            provider: provider.into(),
            severity: sev,
            title: format!("{id} title"),
            detail: format!("{id} detail"),
            repair,
        }
    }

    #[test]
    fn map_severity_skips_ok_and_info() {
        assert_eq!(map_severity(DiagSeverity::Ok), None);
        assert_eq!(map_severity(DiagSeverity::Info), None);
        assert_eq!(map_severity(DiagSeverity::Warn), Some(NotifySeverity::Warn));
        assert_eq!(
            map_severity(DiagSeverity::Crit),
            Some(NotifySeverity::Critical)
        );
    }

    #[test]
    fn in_place_repair_becomes_provider_fix() {
        let spec = RepairSpec {
            id: "install-qemu-guest-agent".into(),
            description: "d".into(),
            automatic: false,
            privileged: true,
            delegate: None,
        };
        let fix = fix_from_repair("proxmox", &spec);
        assert_eq!(fix.provider.as_deref(), Some("proxmox"));
        assert_eq!(fix.repair_id.as_deref(), Some("install-qemu-guest-agent"));
        assert!(fix.unit.is_none() && fix.action.is_none());
    }

    #[test]
    fn delegated_repair_records_unit_and_action() {
        let spec = RepairSpec {
            id: "grow-ram".into(),
            description: "d".into(),
            automatic: false,
            privileged: true,
            delegate: Some(DelegatedRepair {
                unit: UnitId {
                    manager: "proxmox@cluster-a".into(),
                    kind: "lxc".into(),
                    id: "110".into(),
                    name: "mediabox".into(),
                },
                action: "set_resources".into(),
                payload: None,
            }),
        };
        let fix = fix_from_repair("plex", &spec);
        assert_eq!(fix.unit.as_deref(), Some("proxmox@cluster-a/lxc/110"));
        assert_eq!(fix.action.as_deref(), Some("set_resources"));
        assert_eq!(fix.provider.as_deref(), Some("plex"));
    }

    #[test]
    fn raise_input_skips_healthy_and_sets_actionable() {
        assert!(raise_input_for(&finding("ok", "p", DiagSeverity::Ok, None)).is_none());
        assert!(raise_input_for(&finding("info", "p", DiagSeverity::Info, None)).is_none());

        let non_actionable = raise_input_for(&finding("w", "p", DiagSeverity::Warn, None)).unwrap();
        assert!(!non_actionable.actionable);
        assert_eq!(non_actionable.key, "diag:p:w");
        assert_eq!(non_actionable.source, "diagnostics:p");
        assert_eq!(non_actionable.source_ref.as_deref(), Some("w"));

        let spec = RepairSpec {
            id: "fix".into(),
            description: "d".into(),
            automatic: true,
            privileged: false,
            delegate: None,
        };
        let actionable =
            raise_input_for(&finding("c", "p", DiagSeverity::Crit, Some(spec))).unwrap();
        assert!(actionable.actionable);
        assert_eq!(actionable.severity, NotifySeverity::Critical);
        assert!(actionable.fix.is_some());
    }

    fn with_db<T>(f: impl FnOnce() -> T) -> T {
        // Isolate the store on a temp DB so the reconcile can hit the pool.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notify-bridge.db");
        db::with_thread_db_path(&path, || {
            let conn = db::open_default().expect("open temp db");
            drop(conn);
            f()
        })
    }

    #[test]
    fn reconcile_raises_warn_plus_and_skips_healthy() {
        with_db(|| {
            let report = reconcile_with(vec![
                finding("healthy", "p", DiagSeverity::Ok, None),
                finding("advisory", "p", DiagSeverity::Info, None),
                finding("degraded", "p", DiagSeverity::Warn, None),
                finding("broken", "p", DiagSeverity::Crit, None),
            ])
            .unwrap();
            assert_eq!(report.raised, vec!["diag:p:broken", "diag:p:degraded"]);
            assert!(report.cleared.is_empty());
        });
    }

    #[test]
    fn reconcile_auto_dismisses_cleared_findings() {
        with_db(|| {
            // First pass: two findings raise.
            reconcile_with(vec![
                finding("a", "p", DiagSeverity::Crit, None),
                finding("b", "p", DiagSeverity::Warn, None),
            ])
            .unwrap();

            // Second pass: only `a` still fires. `b` cleared → auto-dismissed.
            let report = reconcile_with(vec![finding("a", "p", DiagSeverity::Crit, None)]).unwrap();
            assert_eq!(report.raised, vec!["diag:p:a"]);
            assert_eq!(report.cleared, vec!["diag:p:b"]);

            let b = db::pool::with_pooled_or_open(|c| store::get(c, "diag:p:b"))
                .unwrap()
                .unwrap();
            assert_eq!(b.state, State::Dismissed);
        });
    }

    #[test]
    fn reconcile_leaves_suppressed_rows_alone_when_cleared() {
        with_db(|| {
            reconcile_with(vec![finding("s", "p", DiagSeverity::Warn, None)]).unwrap();
            // User says "ignore permanently".
            db::pool::with_pooled_or_open(|c| store::suppress(c, "diag:p:s", 999)).unwrap();

            // Finding clears; the suppressed row must NOT be auto-dismissed.
            let report = reconcile_with(vec![]).unwrap();
            assert!(report.cleared.is_empty(), "suppressed row is not swept");
            let s = db::pool::with_pooled_or_open(|c| store::get(c, "diag:p:s"))
                .unwrap()
                .unwrap();
            assert_eq!(s.state, State::Suppressed);

            // And a later re-raise stays a no-op (suppressed wins).
            let report = reconcile_with(vec![finding("s", "p", DiagSeverity::Warn, None)]).unwrap();
            assert_eq!(report.raised, vec!["diag:p:s"]);
            let s = db::pool::with_pooled_or_open(|c| store::get(c, "diag:p:s"))
                .unwrap()
                .unwrap();
            assert_eq!(s.state, State::Suppressed);
        });
    }
}
