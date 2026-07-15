//! External-source ingestion reconcile for the stateful notification plane.
//!
//! Companion to `notify_bridge` (which ingests orca's own diagnostics). This
//! pulls notifications from registered
//! [`NotificationSource`](contract::notification_source::NotificationSource)s
//! (unraid, …) and reconciles them into `db::notifications_store`:
//!
//! * Each source is polled. Every returned [`Ingested`] is raised under the key
//!   `<source>:<source_ref>` (idempotent upsert), with `source_ref` retained so
//!   a later dismiss can be pushed back to the source.
//! * A previously-raised row from a source whose ref is **absent** from a
//!   *successful* poll is auto-dismissed — the source cleared it. A poll error
//!   leaves that source's rows untouched (no false clears). User
//!   `dismissed`/`suppressed` rows are left alone.
//!
//! Contract severity/fix are mapped onto the store's own types here — `contract`
//! must not depend on `db`.

use anyhow::Result;
use contract::notification_source::{self, FixLink, Ingested, Severity as SourceSeverity};
use db::notifications_store::{self as store, Fix, RaiseInput, Severity as NotifySeverity, State};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

fn now_ms() -> i64 {
    utils::time::now().unix_millis()
}

fn map_severity(s: SourceSeverity) -> NotifySeverity {
    match s {
        SourceSeverity::Info => NotifySeverity::Info,
        SourceSeverity::Warn => NotifySeverity::Warn,
        SourceSeverity::Error => NotifySeverity::Error,
        SourceSeverity::Critical => NotifySeverity::Critical,
    }
}

fn map_fix(f: FixLink) -> Fix {
    Fix {
        url: f.url,
        provider: f.provider,
        repair_id: f.repair_id,
        unit: f.unit,
        action: f.action,
    }
}

/// Stable key for an ingested notification: `<source>:<source_ref>`.
fn ingest_key(source: &str, source_ref: &str) -> String {
    format!("{source}:{source_ref}")
}

fn raise_input_for(source: &str, ing: Ingested) -> RaiseInput {
    RaiseInput {
        key: ingest_key(source, &ing.source_ref),
        source: source.to_string(),
        source_ref: Some(ing.source_ref),
        severity: map_severity(ing.severity),
        actionable: ing.actionable,
        fix: ing.fix.map(map_fix),
        title: ing.title,
        body: ing.body,
        user_id: None,
    }
}

/// Per-source ingestion outcome.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceIngestReport {
    pub source: String,
    /// Keys raised (created/reactivated) this pass.
    pub raised: Vec<String>,
    /// Keys auto-dismissed because the source no longer reports them.
    pub cleared: Vec<String>,
    /// Set when the source's poll failed; its rows were left untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IngestReport {
    pub sources: Vec<SourceIngestReport>,
}

/// Poll every registered source and reconcile its notifications into the store.
/// Sources are independent: one that errors does not affect the others and does
/// not clear its own rows.
pub async fn ingest_all() -> Result<IngestReport> {
    let mut report = IngestReport::default();
    for src in notification_source::sources() {
        let polled = src.poll().await;
        report.sources.push(ingest_one(src.name(), polled).await?);
    }
    report.sources.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(report)
}

/// Reconcile one source's poll result. Split from the polling so tests drive it
/// without the process-global registry.
async fn ingest_one(source: &str, polled: Result<Vec<Ingested>>) -> Result<SourceIngestReport> {
    let now = now_ms();
    let mut out = SourceIngestReport {
        source: source.to_string(),
        ..Default::default()
    };

    let items = match polled {
        Ok(items) => items,
        Err(e) => {
            // Leave this source's rows untouched — a transient poll failure must
            // not look like every notification cleared.
            out.error = Some(e.to_string());
            return Ok(out);
        }
    };

    let mut seen: HashSet<String> = HashSet::new();
    for ing in items {
        let input = raise_input_for(source, ing);
        seen.insert(input.key.clone());
        let key = input.key.clone();
        db::pool::with_pooled_or_open(|conn| store::raise(conn, input.clone(), now))?;
        out.raised.push(key);
    }

    // Auto-dismiss this source's still-active rows that the source no longer
    // reports. Scope strictly to `source` so we never touch another source's
    // rows; leave user dismissed/suppressed rows alone.
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
            .filter(|n| n.source == source && !seen.contains(&n.key))
            .map(|n| n.key)
            .collect())
    })?;
    for key in stale {
        db::pool::with_pooled_or_open(|conn| store::dismiss(conn, &key, now))?;
        out.cleared.push(key);
    }

    out.raised.sort();
    out.cleared.sort();
    Ok(out)
}

// ── tool ─────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyIngestArgs {}

/// Poll every registered external notification source and reconcile the result
/// into the stateful notification store. Returns per-source raised/cleared keys.
#[orca_tool(domain = "notify", verb = "ingest")]
async fn notify_ingest(_args: NotifyIngestArgs, _ctx: &contract::ToolCtx) -> Result<IngestReport> {
    ingest_all().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ing(source_ref: &str, sev: SourceSeverity) -> Ingested {
        Ingested {
            source_ref: source_ref.into(),
            severity: sev,
            actionable: false,
            title: format!("{source_ref} title"),
            body: None,
            fix: None,
        }
    }

    /// Bind a temp DB on this thread and drive an async closure to completion on
    /// a current-thread runtime *inside* the sync scope, so the thread-local DB
    /// path stays bound across every store access the future makes.
    fn with_db_block<Fut, T>(f: impl FnOnce() -> Fut) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notify-ingest.db");
        db::with_thread_db_path(&path, || {
            let conn = db::open_default().expect("open temp db");
            drop(conn);
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(f())
        })
    }

    #[test]
    fn key_and_mapping() {
        assert_eq!(ingest_key("unraid@h", "42"), "unraid@h:42");
        assert_eq!(map_severity(SourceSeverity::Error), NotifySeverity::Error);
        let input = raise_input_for("unraid@h", ing("42", SourceSeverity::Warn));
        assert_eq!(input.key, "unraid@h:42");
        assert_eq!(input.source, "unraid@h");
        assert_eq!(input.source_ref.as_deref(), Some("42"));
    }

    #[test]
    fn poll_error_leaves_rows_untouched() {
        let (report, state) = with_db_block(|| async {
            db::pool::with_pooled_or_open(|c| {
                store::raise(
                    c,
                    raise_input_for("s@h", ing("1", SourceSeverity::Error)),
                    1,
                )
            })
            .unwrap();
            let r = ingest_one("s@h", Err(anyhow::anyhow!("boom")))
                .await
                .unwrap();
            let still = db::pool::with_pooled_or_open(|c| store::get(c, "s@h:1"))
                .unwrap()
                .unwrap();
            (r, still.state)
        });
        assert!(report.error.is_some(), "poll error recorded");
        assert!(report.cleared.is_empty(), "no clears on poll error");
        assert_eq!(state, State::Active, "existing row untouched on error");
    }

    #[test]
    fn raises_and_auto_dismisses_cleared() {
        let (first, second, one_state, two_state) = with_db_block(|| async {
            let first = ingest_one(
                "s@h",
                Ok(vec![
                    ing("1", SourceSeverity::Error),
                    ing("2", SourceSeverity::Warn),
                ]),
            )
            .await
            .unwrap();
            // Second poll drops ref "2" → it must auto-dismiss.
            let second = ingest_one("s@h", Ok(vec![ing("1", SourceSeverity::Error)]))
                .await
                .unwrap();
            let one = db::pool::with_pooled_or_open(|c| store::get(c, "s@h:1"))
                .unwrap()
                .unwrap();
            let two = db::pool::with_pooled_or_open(|c| store::get(c, "s@h:2"))
                .unwrap()
                .unwrap();
            (first, second, one.state, two.state)
        });
        assert_eq!(first.raised, vec!["s@h:1", "s@h:2"]);
        assert_eq!(second.raised, vec!["s@h:1"]);
        assert_eq!(second.cleared, vec!["s@h:2"]);
        assert_eq!(one_state, State::Active);
        assert_eq!(two_state, State::Dismissed);
    }

    #[test]
    fn does_not_clear_other_sources_rows() {
        let (report, other_state) = with_db_block(|| async {
            // A row owned by a different source.
            db::pool::with_pooled_or_open(|c| {
                store::raise(
                    c,
                    raise_input_for("other@h", ing("9", SourceSeverity::Error)),
                    1,
                )
            })
            .unwrap();
            // Reconcile s@h with an empty poll — must not touch other@h.
            let report = ingest_one("s@h", Ok(vec![])).await.unwrap();
            let other = db::pool::with_pooled_or_open(|c| store::get(c, "other@h:9"))
                .unwrap()
                .unwrap();
            (report, other.state)
        });
        assert!(report.cleared.is_empty());
        assert_eq!(other_state, State::Active, "other source's row untouched");
    }
}
