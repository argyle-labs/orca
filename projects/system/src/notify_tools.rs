//! Dismissable-notification tools: `notify.raise`, `notify.list`,
//! `notify.dismiss`, `notify.suppress`.
//!
//! These drive the STATEFUL notification plane (see
//! `db::notifications_store`), complementing the EPHEMERAL `notify.send`
//! (fire-and-forget fan-out, in the `notifications` crate). A raised
//! notification persists with a lifecycle and an *audience*; user-audience
//! raises are additionally fanned once through the ephemeral dispatcher so
//! they reach the user's configured backends immediately.

use db::notifications_store as store;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use store::{Audience, Fix, RaiseInput, Severity, State};

fn now_ms() -> i64 {
    utils::time::now().unix_millis()
}

// ── Serializable view ─────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FixView {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repair_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub action: Option<String>,
}

impl From<Fix> for FixView {
    fn from(f: Fix) -> Self {
        FixView {
            url: f.url,
            provider: f.provider,
            repair_id: f.repair_id,
            unit: f.unit,
            action: f.action,
        }
    }
}

impl From<FixView> for Fix {
    fn from(f: FixView) -> Self {
        Fix {
            url: f.url,
            provider: f.provider,
            repair_id: f.repair_id,
            unit: f.unit,
            action: f.action,
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NotificationView {
    pub key: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// `info` | `warn` | `error` | `critical`.
    pub severity: String,
    pub actionable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixView>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `user` | `system`.
    pub audience: String,
    /// `active` | `dismissed` | `suppressed`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

impl From<store::Notification> for NotificationView {
    fn from(n: store::Notification) -> Self {
        NotificationView {
            key: n.key,
            source: n.source,
            source_ref: n.source_ref,
            severity: n.severity.as_str().to_string(),
            actionable: n.actionable,
            fix: n.fix.map(Into::into),
            title: n.title,
            body: n.body,
            audience: n.audience.as_str().to_string(),
            state: n.state.as_str().to_string(),
            user_id: n.user_id,
            created_at: n.created_at,
            updated_at: n.updated_at,
        }
    }
}

// ── raise ──────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyRaiseArgs {
    /// Stable dedup id, e.g. `unraid:<host>:<src_id>` or `diag:<provider>:<id>`.
    /// Re-raising the same key upserts + reactivates; a suppressed key is a no-op.
    #[arg(long)]
    pub key: String,
    /// Origin, e.g. `unraid@<host>` or `diagnostics:proxmox`.
    #[arg(long)]
    pub source: String,
    /// The source's own id for this notification (enables dismiss-at-source).
    #[arg(long = "source-ref")]
    pub source_ref: Option<String>,
    /// `info` | `warn` | `error` | `critical`. Defaults to `info`.
    #[arg(long)]
    pub severity: Option<String>,
    /// Whether the user can act on this. Drives audience + surfaces the fix link.
    #[arg(long, default_value_t = false)]
    pub actionable: bool,
    /// Optional remediation link (external URL and/or in-orca deep link).
    #[arg(skip)]
    pub fix: Option<FixView>,
    #[arg(long)]
    pub title: String,
    #[arg(long)]
    pub body: Option<String>,
    /// Optional user targeting.
    #[arg(long = "user-id")]
    pub user_id: Option<String>,
}

/// Raise (create or reactivate) a dismissable notification. Idempotent on
/// `key`. Returns the persisted row; `audience` is derived (user iff
/// severity>=error OR actionable). User-audience raises also fan once through
/// the ephemeral dispatcher.
#[orca_tool(domain = "notify", verb = "raise")]
async fn notify_raise(
    args: NotifyRaiseArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotificationView> {
    let severity = Severity::parse(args.severity.as_deref().unwrap_or("info"))?;
    let input = RaiseInput {
        key: args.key,
        source: args.source,
        source_ref: args.source_ref,
        severity,
        actionable: args.actionable,
        fix: args.fix.map(Into::into),
        title: args.title,
        body: args.body,
        user_id: args.user_id,
    };
    let now = now_ms();
    let raised = db::pool::with_pooled_or_open(|conn| store::raise(conn, input.clone(), now))?;

    // User-plane notifications fan once through the ephemeral dispatcher so
    // they hit the user's configured backends (ntfy/slack) on raise. Only for
    // freshly-active user-audience rows — a suppressed no-op must stay silent.
    if raised.audience == Audience::User && raised.state == State::Active {
        fan_ephemeral(&raised).await;
    }

    Ok(raised.into())
}

/// Emit a raised user-audience notification through the ephemeral dispatcher.
/// Best-effort: an unconfigured host (no backends) is a silent no-op.
async fn fan_ephemeral(n: &store::Notification) {
    use notifications::{Event, EventClass, Severity as ESeverity};
    if notifications::registered_backend_names().is_empty() {
        return;
    }
    let severity = match n.severity {
        Severity::Info => ESeverity::Info,
        Severity::Warn => ESeverity::Warn,
        Severity::Error => ESeverity::Error,
        Severity::Critical => ESeverity::Critical,
    };
    let mut event = Event::new(
        EventClass::Alert,
        severity,
        n.title.clone(),
        n.source.clone(),
    );
    if let Some(body) = &n.body {
        event = event.with_body(body.clone());
    }
    // Surface the fix as the click target when it carries an external URL.
    if let Some(url) = n.fix.as_ref().and_then(|f| f.url.clone()) {
        event = event.with_click(url);
    }
    let _ = notifications::emit(&event).await;
}

// ── list ─────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyListArgs {
    /// Filter by lifecycle state: `active` | `dismissed` | `suppressed`.
    /// Omit for all states.
    #[arg(long)]
    pub state: Option<String>,
    /// Filter by audience: `user` | `system`. Omit for both.
    #[arg(long)]
    pub audience: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct NotifyListOutput {
    pub notifications: Vec<NotificationView>,
}

/// List dismissable notifications, newest first. Filters are ANDed.
#[orca_tool(domain = "notify", verb = "list")]
async fn notify_list(
    args: NotifyListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotifyListOutput> {
    let filter = store::ListFilter {
        state: args.state.as_deref().map(State::parse).transpose()?,
        audience: args.audience.as_deref().map(Audience::parse).transpose()?,
    };
    let rows = db::pool::with_pooled_or_open(|conn| store::list(conn, &filter))?;
    Ok(NotifyListOutput {
        notifications: rows.into_iter().map(Into::into).collect(),
    })
}

// ── dismiss / suppress ─────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyKeyArgs {
    /// Key of the notification to act on.
    #[arg(long)]
    pub key: String,
}

/// Outcome of pushing a dismiss back to the originating external source.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SourceDismissResult {
    /// The source the dismiss was routed to.
    pub source: String,
    /// Whether the source acknowledged the dismiss.
    pub ok: bool,
    /// Failure detail when `ok == false`. The local dismiss still stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct NotifyMutateOutput {
    /// The updated notification, or `null` if no notification had that key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification: Option<NotificationView>,
    /// Present when the dismissed notification was pushed back to its external
    /// source (the source is registered, supports dismiss-at-source, and the
    /// row carries a `source_ref`). Absent for local-only dismisses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_dismiss: Option<SourceDismissResult>,
}

/// Dismiss a notification (user acknowledged it). A later re-raise of the same
/// key reactivates it. If the notification came from an external source that
/// supports dismiss-at-source, the dismiss is also pushed back to that source
/// (best-effort — a source failure is reported but the local dismiss stands).
#[orca_tool(domain = "notify", verb = "dismiss")]
async fn notify_dismiss(
    args: NotifyKeyArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotifyMutateOutput> {
    let now = now_ms();
    let updated = db::pool::with_pooled_or_open(|conn| store::dismiss(conn, &args.key, now))?;
    let source_dismiss = match &updated {
        Some(n) => dismiss_at_source(n).await,
        None => None,
    };
    Ok(NotifyMutateOutput {
        notification: updated.map(Into::into),
        source_dismiss,
    })
}

/// Push a dismiss back to the notification's external source, when one is
/// registered for it, supports remote dismiss, and the row carries a
/// `source_ref`. Returns `None` when there is nothing to push (a local-only or
/// diagnostics-originated notification).
async fn dismiss_at_source(n: &store::Notification) -> Option<SourceDismissResult> {
    let source_ref = n.source_ref.as_ref()?;
    let src = contract::notification_source::source(&n.source)?;
    if !src.supports_dismiss_at_source() {
        return None;
    }
    let (ok, error) = match src.dismiss_at_source(source_ref).await {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    Some(SourceDismissResult {
        source: n.source.clone(),
        ok,
        error,
    })
}

/// Suppress a notification permanently ("ignore permanently"). Re-raises of the
/// same key become no-ops until the row is deleted.
#[orca_tool(domain = "notify", verb = "suppress")]
async fn notify_suppress(
    args: NotifyKeyArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotifyMutateOutput> {
    let now = now_ms();
    let updated = db::pool::with_pooled_or_open(|conn| store::suppress(conn, &args.key, now))?;
    Ok(NotifyMutateOutput {
        notification: updated.map(Into::into),
        source_dismiss: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_view_round_trips_to_store_fix() {
        let v = FixView {
            provider: Some("proxmox".into()),
            repair_id: Some("install-qemu-guest-agent".into()),
            url: None,
            unit: None,
            action: None,
        };
        let back: FixView = Fix::from(v.clone()).into();
        assert_eq!(back, v);
    }

    #[test]
    fn notification_view_serializes_camel_case() {
        let n = store::Notification {
            key: "diag:proxmox:x".into(),
            source: "diagnostics:proxmox".into(),
            source_ref: None,
            severity: Severity::Error,
            actionable: true,
            fix: None,
            title: "t".into(),
            body: None,
            audience: Audience::User,
            state: State::Active,
            user_id: None,
            created_at: 1,
            updated_at: 2,
        };
        let v = serde_json::to_value(NotificationView::from(n)).unwrap();
        assert_eq!(v["severity"], "error");
        assert_eq!(v["audience"], "user");
        assert_eq!(v["state"], "active");
        assert_eq!(v["actionable"], true);
        assert_eq!(v["createdAt"], 1);
        assert_eq!(v["updatedAt"], 2);
        // Absent optionals are skipped.
        assert!(v.get("sourceRef").is_none());
        assert!(v.get("fix").is_none());
    }

    #[test]
    fn list_args_default_no_filters() {
        let a = NotifyListArgs::default();
        assert!(a.state.is_none() && a.audience.is_none());
    }

    #[test]
    fn list_args_deserialize_camel_case() {
        let a: NotifyListArgs =
            serde_json::from_str(r#"{"state":"active","audience":"user"}"#).unwrap();
        assert_eq!(a.state.as_deref(), Some("active"));
        assert_eq!(a.audience.as_deref(), Some("user"));
    }
}
