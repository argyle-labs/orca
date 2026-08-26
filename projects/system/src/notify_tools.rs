//! Dismissable-notification tools: `notify.list`, plus the merged
//! `notify.create{action=raise|ingest|send}` and
//! `notify.update{action=dismiss|suppress|sync_diagnostics}` dispatchers.
//!
//! These drive the STATEFUL notification plane (see
//! `db::notifications_store`), complementing the EPHEMERAL send path
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
/// the ephemeral dispatcher. Reached via `notify.create{action=raise}`.
async fn notify_raise(args: NotifyRaiseArgs) -> anyhow::Result<NotificationView> {
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
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct NotifyListOutput {
    pub notifications: Vec<NotificationView>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total notifications across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
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
    let notifications: Vec<NotificationView> = rows.into_iter().map(Into::into).collect();
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(notifications, &params);
    Ok(NotifyListOutput {
        notifications: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
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
/// Reached via `notify.update{action=dismiss}`.
async fn notify_dismiss(args: NotifyKeyArgs) -> anyhow::Result<NotifyMutateOutput> {
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
/// same key become no-ops until the row is deleted. Reached via
/// `notify.update{action=suppress}`.
async fn notify_suppress(args: NotifyKeyArgs) -> anyhow::Result<NotifyMutateOutput> {
    let now = now_ms();
    let updated = db::pool::with_pooled_or_open(|conn| store::suppress(conn, &args.key, now))?;
    Ok(NotifyMutateOutput {
        notification: updated.map(Into::into),
        source_dismiss: None,
    })
}

// ── create{action=raise|ingest|send} ─────────────────────────────────────────

/// The `notify.create` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum NotifyCreateAction {
    /// Raise (create or reactivate) a stateful dismissable notification.
    Raise,
    /// Poll every registered external source and reconcile into the store.
    Ingest,
    /// Fire an ephemeral event through the installed dispatcher (no state).
    Send,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyCreateArgs {
    /// Which create action to run: `raise`, `ingest`, or `send`.
    #[arg(long, value_enum)]
    pub action: Option<NotifyCreateAction>,
    /// `raise`: stable dedup id. `send`: ignored.
    #[arg(long)]
    pub key: Option<String>,
    /// `raise`/`send`: origin, e.g. `unraid@<host>` or `diagnostics:proxmox`.
    #[arg(long)]
    pub source: Option<String>,
    /// `raise`: the source's own id for this notification.
    #[arg(long = "source-ref")]
    pub source_ref: Option<String>,
    /// `raise`/`send`: severity `info`|`warn`|`error`|`critical`. Defaults to `info`.
    #[arg(long)]
    pub severity: Option<String>,
    /// `raise`: whether the user can act on this.
    #[arg(long, default_value_t = false)]
    pub actionable: bool,
    /// `raise`: optional remediation link.
    #[arg(skip)]
    pub fix: Option<FixView>,
    /// `raise`/`send`: notification title (required for both).
    #[arg(long)]
    pub title: Option<String>,
    /// `raise`/`send`: optional body.
    #[arg(long)]
    pub body: Option<String>,
    /// `raise`: optional user targeting.
    #[arg(long = "user-id")]
    pub user_id: Option<String>,
    /// `send`: event class — `heartbeat`|`drift`|`rotation`|`lifecycle`|`alert`|
    /// `approval`. Defaults to `alert`.
    #[arg(long)]
    pub class: Option<String>,
    /// `send`: host this event is about (not necessarily this host).
    #[arg(long)]
    pub host: Option<String>,
    /// `send`: optional tap-through URL surfaced as the click target.
    #[arg(long)]
    pub click: Option<String>,
}

/// `notify.create` payload — one variant per `action`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NotifyCreateOutput {
    Raise(Box<NotificationView>),
    Ingest(crate::notify_ingest::IngestReport),
    Send(notifications::notify_send::NotifySendOutput),
}

/// Create a notification. `action=raise` upserts a stateful dismissable
/// notification (idempotent on `key`); `action=ingest` polls every registered
/// external source and reconciles into the store; `action=send` fires an
/// ephemeral event through the installed dispatcher (no persistent state).
#[orca_tool(domain = "notify", verb = "create")]
async fn notify_create(
    args: NotifyCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotifyCreateOutput> {
    let action = args.action.ok_or_else(|| {
        anyhow::anyhow!("`action` is required for notify.create (raise|ingest|send)")
    })?;
    match action {
        NotifyCreateAction::Raise => {
            let raise = NotifyRaiseArgs {
                key: args
                    .key
                    .ok_or_else(|| anyhow::anyhow!("`key` is required for action=raise"))?,
                source: args
                    .source
                    .ok_or_else(|| anyhow::anyhow!("`source` is required for action=raise"))?,
                source_ref: args.source_ref,
                severity: args.severity,
                actionable: args.actionable,
                fix: args.fix,
                title: args
                    .title
                    .ok_or_else(|| anyhow::anyhow!("`title` is required for action=raise"))?,
                body: args.body,
                user_id: args.user_id,
            };
            Ok(NotifyCreateOutput::Raise(Box::new(
                notify_raise(raise).await?,
            )))
        }
        NotifyCreateAction::Ingest => Ok(NotifyCreateOutput::Ingest(
            crate::notify_ingest::ingest_all().await?,
        )),
        NotifyCreateAction::Send => {
            let send = notifications::notify_send::NotifySendArgs {
                class: args.class,
                severity: args.severity,
                title: args
                    .title
                    .ok_or_else(|| anyhow::anyhow!("`title` is required for action=send"))?,
                body: args.body,
                host: args.host,
                source: args.source,
                click: args.click,
            };
            Ok(NotifyCreateOutput::Send(
                notifications::notify_send::send(send).await?,
            ))
        }
    }
}

// ── update{action=dismiss|suppress|sync_diagnostics} ─────────────────────────

/// The `notify.update` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum NotifyUpdateAction {
    /// Dismiss a notification (user acknowledged it).
    Dismiss,
    /// Suppress a notification permanently ("ignore permanently").
    Suppress,
    /// Run the diagnostics→notification reconcile pass.
    SyncDiagnostics,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyUpdateArgs {
    /// Which update action to run.
    #[arg(long, value_enum)]
    pub action: Option<NotifyUpdateAction>,
    /// `dismiss`/`suppress`: key of the notification to act on.
    #[arg(long)]
    pub key: Option<String>,
}

/// `notify.update` payload — one variant per `action`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NotifyUpdateOutput {
    Mutate(Box<NotifyMutateOutput>),
    SyncDiagnostics(crate::notify_bridge::BridgeReport),
}

/// Update notification state. `action=dismiss` acknowledges a notification (a
/// later re-raise reactivates it, and an external source that supports it is
/// pushed the dismiss); `action=suppress` ignores it permanently; and
/// `action=sync_diagnostics` runs the diagnostics→notification reconcile pass.
#[orca_tool(domain = "notify", verb = "update")]
async fn notify_update(
    args: NotifyUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotifyUpdateOutput> {
    let action = args.action.ok_or_else(|| {
        anyhow::anyhow!(
            "`action` is required for notify.update (dismiss|suppress|sync_diagnostics)"
        )
    })?;
    match action {
        NotifyUpdateAction::Dismiss => {
            let key = args
                .key
                .ok_or_else(|| anyhow::anyhow!("`key` is required for action=dismiss"))?;
            Ok(NotifyUpdateOutput::Mutate(Box::new(
                notify_dismiss(NotifyKeyArgs { key }).await?,
            )))
        }
        NotifyUpdateAction::Suppress => {
            let key = args
                .key
                .ok_or_else(|| anyhow::anyhow!("`key` is required for action=suppress"))?;
            Ok(NotifyUpdateOutput::Mutate(Box::new(
                notify_suppress(NotifyKeyArgs { key }).await?,
            )))
        }
        NotifyUpdateAction::SyncDiagnostics => Ok(NotifyUpdateOutput::SyncDiagnostics(
            crate::notify_bridge::reconcile_diagnostics().await?,
        )),
    }
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

    // ── ToolCtx for the async error branches (no DB hit) ──────────────────────
    // The action-dispatch guards short-circuit BEFORE touching the store, so
    // these exercise real code paths without a live DB. A unique per-invocation
    // temp dir mirrors the crate's `empty_ctx()` pattern.
    fn empty_ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        use std::sync::Arc;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("orca-notify-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("create temp ctx dir");
        contract::ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("notify-test.db"),
            ports: Default::default(),
        }))
    }

    // ── FixView full round-trip (all fields Some) ─────────────────────────────

    #[test]
    fn fix_view_round_trips_with_all_fields_set() {
        let v = FixView {
            url: Some("https://x/fix".into()),
            provider: Some("unraid".into()),
            repair_id: Some("r-1".into()),
            unit: Some("nfsd".into()),
            action: Some("restart".into()),
        };
        let back: FixView = Fix::from(v.clone()).into();
        assert_eq!(back, v);
    }

    // ── NotificationView serde with all optionals present ─────────────────────

    #[test]
    fn notification_view_serializes_all_optionals_and_mappings() {
        let n = store::Notification {
            key: "unraid:host:1".into(),
            source: "unraid@host".into(),
            source_ref: Some("src-42".into()),
            severity: Severity::Critical,
            actionable: false,
            fix: Some(Fix {
                url: Some("https://x".into()),
                provider: Some("unraid".into()),
                repair_id: None,
                unit: None,
                action: None,
            }),
            title: "disk full".into(),
            body: Some("act now".into()),
            audience: Audience::System,
            state: State::Dismissed,
            user_id: Some("u-9".into()),
            created_at: 10,
            updated_at: 20,
        };
        let s = serde_json::to_string(&NotificationView::from(n)).unwrap();
        assert!(s.contains("\"severity\":\"critical\""));
        assert!(s.contains("\"audience\":\"system\""));
        assert!(s.contains("\"state\":\"dismissed\""));
        assert!(s.contains("\"sourceRef\":\"src-42\""));
        assert!(s.contains("\"userId\":\"u-9\""));
        assert!(s.contains("\"body\":\"act now\""));
        assert!(s.contains("\"fix\":"));
        assert!(s.contains("\"createdAt\":10"));
        assert!(s.contains("\"updatedAt\":20"));
    }

    #[test]
    fn notification_view_maps_warn_and_suppressed() {
        let n = store::Notification {
            key: "k".into(),
            source: "s".into(),
            source_ref: None,
            severity: Severity::Warn,
            actionable: false,
            fix: None,
            title: "t".into(),
            body: None,
            audience: Audience::System,
            state: State::Suppressed,
            user_id: None,
            created_at: 0,
            updated_at: 0,
        };
        let view = NotificationView::from(n);
        assert_eq!(view.severity, "warn");
        assert_eq!(view.state, "suppressed");
        assert_eq!(view.audience, "system");
    }

    // ── action enum serde (snake_case) ────────────────────────────────────────

    #[test]
    fn create_action_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&NotifyCreateAction::Raise).unwrap(),
            "\"raise\""
        );
        assert_eq!(
            serde_json::to_string(&NotifyCreateAction::Ingest).unwrap(),
            "\"ingest\""
        );
        assert_eq!(
            serde_json::to_string(&NotifyCreateAction::Send).unwrap(),
            "\"send\""
        );
    }

    #[test]
    fn update_action_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&NotifyUpdateAction::Dismiss).unwrap(),
            "\"dismiss\""
        );
        assert_eq!(
            serde_json::to_string(&NotifyUpdateAction::Suppress).unwrap(),
            "\"suppress\""
        );
        assert_eq!(
            serde_json::to_string(&NotifyUpdateAction::SyncDiagnostics).unwrap(),
            "\"sync_diagnostics\""
        );
    }

    #[test]
    fn create_action_deserializes_snake_case() {
        let a: NotifyCreateAction = serde_json::from_str("\"ingest\"").unwrap();
        assert_eq!(a, NotifyCreateAction::Ingest);
        let a: NotifyUpdateAction = serde_json::from_str("\"sync_diagnostics\"").unwrap();
        assert_eq!(a, NotifyUpdateAction::SyncDiagnostics);
    }

    // ── args defaults + camelCase deserialize ─────────────────────────────────

    #[test]
    fn raise_args_default_and_deserialize() {
        let d = NotifyRaiseArgs::default();
        assert!(d.key.is_empty() && d.source.is_empty() && !d.actionable);
        let a: NotifyRaiseArgs = serde_json::from_str(
            r#"{"key":"k","source":"s","sourceRef":"r","severity":"error","actionable":true,"title":"t","userId":"u"}"#,
        )
        .unwrap();
        assert_eq!(a.key, "k");
        assert_eq!(a.source_ref.as_deref(), Some("r"));
        assert!(a.actionable);
        assert_eq!(a.user_id.as_deref(), Some("u"));
    }

    #[test]
    fn create_args_default_and_deserialize() {
        let d = NotifyCreateArgs::default();
        assert!(d.action.is_none() && d.key.is_none() && !d.actionable);
        let a: NotifyCreateArgs =
            serde_json::from_str(r#"{"action":"raise","key":"k","source":"s","title":"t"}"#)
                .unwrap();
        assert_eq!(a.action, Some(NotifyCreateAction::Raise));
        assert_eq!(a.key.as_deref(), Some("k"));
    }

    #[test]
    fn update_args_default_and_deserialize() {
        let d = NotifyUpdateArgs::default();
        assert!(d.action.is_none() && d.key.is_none());
        let a: NotifyUpdateArgs =
            serde_json::from_str(r#"{"action":"dismiss","key":"k"}"#).unwrap();
        assert_eq!(a.action, Some(NotifyUpdateAction::Dismiss));
        assert_eq!(a.key.as_deref(), Some("k"));
    }

    // ── NotifyListOutput skips None cursor/total ──────────────────────────────

    #[test]
    fn list_output_skips_none_cursor_and_total() {
        let out = NotifyListOutput {
            notifications: vec![],
            next_cursor: None,
            total: None,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("nextCursor"));
        assert!(!s.contains("total"));
        assert!(s.contains("\"notifications\":[]"));
    }

    #[test]
    fn list_output_emits_cursor_and_total_when_set() {
        let out = NotifyListOutput {
            notifications: vec![],
            next_cursor: Some("c".into()),
            total: Some(3),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("\"nextCursor\":\"c\""));
        assert!(s.contains("\"total\":3"));
    }

    // ── SourceDismissResult / NotifyMutateOutput serde ────────────────────────

    #[test]
    fn source_dismiss_result_skips_none_error() {
        let ok = SourceDismissResult {
            source: "unraid@host".into(),
            ok: true,
            error: None,
        };
        let s = serde_json::to_string(&ok).unwrap();
        assert!(s.contains("\"ok\":true"));
        assert!(!s.contains("error"));
    }

    #[test]
    fn mutate_output_skips_none_fields() {
        let out = NotifyMutateOutput {
            notification: None,
            source_dismiss: None,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert_eq!(s, "{}");
    }

    // ── now_ms ────────────────────────────────────────────────────────────────

    #[test]
    fn now_ms_is_positive() {
        assert!(now_ms() > 0);
    }

    // ── async dispatcher error guards (no DB hit) ─────────────────────────────

    #[tokio::test]
    async fn notify_create_requires_action() {
        let ctx = empty_ctx();
        let Err(err) = notify_create(NotifyCreateArgs::default(), &ctx).await else {
            panic!("expected error");
        };
        assert!(err.to_string().contains("action"));
    }

    #[tokio::test]
    async fn notify_create_raise_requires_key() {
        let ctx = empty_ctx();
        let args = NotifyCreateArgs {
            action: Some(NotifyCreateAction::Raise),
            ..Default::default()
        };
        let Err(err) = notify_create(args, &ctx).await else {
            panic!("expected error");
        };
        assert!(err.to_string().contains("key"));
    }

    #[tokio::test]
    async fn notify_create_raise_requires_source_after_key() {
        let ctx = empty_ctx();
        let args = NotifyCreateArgs {
            action: Some(NotifyCreateAction::Raise),
            key: Some("k".into()),
            ..Default::default()
        };
        let Err(err) = notify_create(args, &ctx).await else {
            panic!("expected error");
        };
        assert!(err.to_string().contains("source"));
    }

    #[tokio::test]
    async fn notify_create_send_requires_title() {
        let ctx = empty_ctx();
        let args = NotifyCreateArgs {
            action: Some(NotifyCreateAction::Send),
            ..Default::default()
        };
        let Err(err) = notify_create(args, &ctx).await else {
            panic!("expected error");
        };
        assert!(err.to_string().contains("title"));
    }

    #[tokio::test]
    async fn notify_update_requires_action() {
        let ctx = empty_ctx();
        let Err(err) = notify_update(NotifyUpdateArgs::default(), &ctx).await else {
            panic!("expected error");
        };
        assert!(err.to_string().contains("action"));
    }

    #[tokio::test]
    async fn notify_update_dismiss_requires_key() {
        let ctx = empty_ctx();
        let args = NotifyUpdateArgs {
            action: Some(NotifyUpdateAction::Dismiss),
            key: None,
        };
        let Err(err) = notify_update(args, &ctx).await else {
            panic!("expected error");
        };
        assert!(err.to_string().contains("key"));
    }

    #[tokio::test]
    async fn notify_update_suppress_requires_key() {
        let ctx = empty_ctx();
        let args = NotifyUpdateArgs {
            action: Some(NotifyUpdateAction::Suppress),
            key: None,
        };
        let Err(err) = notify_update(args, &ctx).await else {
            panic!("expected error");
        };
        assert!(err.to_string().contains("key"));
    }

    // ── notify_raise: severity parse error path (no DB reached) ────────────────

    #[tokio::test]
    async fn notify_raise_rejects_bad_severity() {
        let args = NotifyRaiseArgs {
            key: "k".into(),
            source: "s".into(),
            severity: Some("not-a-severity".into()),
            title: "t".into(),
            ..Default::default()
        };
        let err = notify_raise(args).await.unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    // ── success paths against a real (temp, migrated) SQLite DB ───────────────
    // `db::with_db_path` scopes an ephemeral unencrypted DB — every
    // `open_default()`/`with_pooled_or_open` inside the future opens the temp
    // file (schema + migrations applied on first open), so these drive the
    // full store round-trip, not just the guard branches.

    fn tmp_db_path() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orca-notify-db-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("create temp db dir");
        dir.join("notify.db")
    }

    #[tokio::test]
    async fn notify_raise_persists_system_audience_no_fan() {
        let path = tmp_db_path();
        db::with_db_path(path, async {
            // Non-actionable info stays system-audience → no ephemeral fan.
            let args = NotifyRaiseArgs {
                key: "unraid:host:1".into(),
                source: "unraid@host".into(),
                severity: Some("warn".into()),
                actionable: false,
                title: "disk warm".into(),
                body: Some("watch it".into()),
                ..Default::default()
            };
            let view = notify_raise(args).await.expect("raise ok");
            assert_eq!(view.key, "unraid:host:1");
            assert_eq!(view.audience, "system");
            assert_eq!(view.state, "active");
            assert_eq!(view.severity, "warn");
            assert_eq!(view.body.as_deref(), Some("watch it"));
            assert!(view.created_at > 0);
        })
        .await;
    }

    #[tokio::test]
    async fn notify_raise_user_audience_fans_through_backend() {
        let path = tmp_db_path();
        // A registered backend takes fan_ephemeral past its no-backend early
        // return, exercising the event-build (body + fix click) path. Backends
        // are process-global; nextest isolates each test in its own process,
        // but deregister anyway to keep `cargo test` clean.
        use std::sync::Arc;
        notifications::register_from_def(
            "test-sink".into(),
            Arc::new(|_op: &str, _args: String| {
                Ok("{\"backend\":\"test-sink\",\"id\":\"m1\"}".to_string())
            }),
        )
        .expect("register backend");
        db::with_db_path(path, async {
            let args = NotifyRaiseArgs {
                key: "diag:proxmox:agent".into(),
                source: "diagnostics:proxmox".into(),
                severity: Some("critical".into()),
                actionable: true,
                fix: Some(FixView {
                    url: Some("https://orca/fix/agent".into()),
                    provider: Some("proxmox".into()),
                    repair_id: Some("install-agent".into()),
                    unit: None,
                    action: None,
                }),
                title: "agent missing".into(),
                body: Some("install it".into()),
                ..Default::default()
            };
            let view = notify_raise(args).await.expect("raise ok");
            assert_eq!(view.audience, "user");
            assert_eq!(view.state, "active");
            assert_eq!(view.severity, "critical");
            assert!(view.actionable);
            assert_eq!(
                view.fix.as_ref().and_then(|f| f.url.as_deref()),
                Some("https://orca/fix/agent")
            );
        })
        .await;
        assert!(notifications::deregister_backend("test-sink"));
    }

    #[tokio::test]
    async fn notify_list_returns_raised_rows() {
        let path = tmp_db_path();
        db::with_db_path(path, async {
            for (k, sev) in [("a", "info"), ("b", "error")] {
                notify_raise(NotifyRaiseArgs {
                    key: k.into(),
                    source: "s".into(),
                    severity: Some(sev.into()),
                    title: k.into(),
                    ..Default::default()
                })
                .await
                .expect("raise ok");
            }
            let ctx = empty_ctx();
            let out = notify_list(NotifyListArgs::default(), &ctx)
                .await
                .expect("list ok");
            assert_eq!(out.total, Some(2));
            assert_eq!(out.notifications.len(), 2);

            // Audience filter narrows to the error (user-audience) row only.
            let filtered = notify_list(
                NotifyListArgs {
                    audience: Some("user".into()),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .expect("list ok");
            assert_eq!(filtered.total, Some(1));
            assert_eq!(filtered.notifications[0].key, "b");
        })
        .await;
    }

    #[tokio::test]
    async fn notify_list_rejects_bad_state_filter() {
        let ctx = empty_ctx();
        let err = notify_list(
            NotifyListArgs {
                state: Some("nonsense".into()),
                ..Default::default()
            },
            &ctx,
        )
        .await
        .expect_err("bad state must error");
        assert!(!err.to_string().is_empty());
    }

    #[tokio::test]
    async fn notify_dismiss_existing_returns_updated_no_source_push() {
        let path = tmp_db_path();
        db::with_db_path(path, async {
            notify_raise(NotifyRaiseArgs {
                key: "k1".into(),
                source: "diagnostics:proxmox".into(),
                source_ref: Some("ref-1".into()),
                severity: Some("error".into()),
                title: "t".into(),
                ..Default::default()
            })
            .await
            .expect("raise ok");
            let out = notify_dismiss(NotifyKeyArgs { key: "k1".into() })
                .await
                .expect("dismiss ok");
            let n = out.notification.expect("row present");
            assert_eq!(n.state, "dismissed");
            // `diagnostics:*` is not a registered notification source → no push.
            assert!(out.source_dismiss.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn notify_dismiss_missing_key_yields_null() {
        let path = tmp_db_path();
        db::with_db_path(path, async {
            let out = notify_dismiss(NotifyKeyArgs { key: "nope".into() })
                .await
                .expect("dismiss ok");
            assert!(out.notification.is_none());
            assert!(out.source_dismiss.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn notify_suppress_existing_then_reraise_is_noop() {
        let path = tmp_db_path();
        db::with_db_path(path, async {
            notify_raise(NotifyRaiseArgs {
                key: "k2".into(),
                source: "s".into(),
                severity: Some("warn".into()),
                title: "t".into(),
                ..Default::default()
            })
            .await
            .expect("raise ok");
            let out = notify_suppress(NotifyKeyArgs { key: "k2".into() })
                .await
                .expect("suppress ok");
            assert_eq!(out.notification.expect("row").state, "suppressed");
            assert!(out.source_dismiss.is_none());

            // A re-raise of a suppressed key is a no-op: stays suppressed.
            let re = notify_raise(NotifyRaiseArgs {
                key: "k2".into(),
                source: "s".into(),
                severity: Some("critical".into()),
                actionable: true,
                title: "again".into(),
                ..Default::default()
            })
            .await
            .expect("raise ok");
            assert_eq!(re.state, "suppressed");
        })
        .await;
    }

    #[tokio::test]
    async fn notify_create_raise_dispatch_full_success() {
        let path = tmp_db_path();
        db::with_db_path(path, async {
            let ctx = empty_ctx();
            let out = notify_create(
                NotifyCreateArgs {
                    action: Some(NotifyCreateAction::Raise),
                    key: Some("ck".into()),
                    source: Some("s".into()),
                    severity: Some("info".into()),
                    title: Some("hello".into()),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .expect("create ok");
            match out {
                NotifyCreateOutput::Raise(v) => {
                    assert_eq!(v.key, "ck");
                    assert_eq!(v.state, "active");
                    assert_eq!(v.audience, "system");
                }
                _ => panic!("expected Raise variant"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn notify_update_dismiss_and_suppress_dispatch_full_success() {
        let path = tmp_db_path();
        db::with_db_path(path, async {
            let ctx = empty_ctx();
            notify_raise(NotifyRaiseArgs {
                key: "uk".into(),
                source: "s".into(),
                severity: Some("warn".into()),
                title: "t".into(),
                ..Default::default()
            })
            .await
            .expect("raise ok");

            let out = notify_update(
                NotifyUpdateArgs {
                    action: Some(NotifyUpdateAction::Dismiss),
                    key: Some("uk".into()),
                },
                &ctx,
            )
            .await
            .expect("update ok");
            match out {
                NotifyUpdateOutput::Mutate(m) => {
                    assert_eq!(m.notification.expect("row").state, "dismissed");
                }
                _ => panic!("expected Mutate variant"),
            }

            let out = notify_update(
                NotifyUpdateArgs {
                    action: Some(NotifyUpdateAction::Suppress),
                    key: Some("uk".into()),
                },
                &ctx,
            )
            .await
            .expect("update ok");
            match out {
                NotifyUpdateOutput::Mutate(m) => {
                    assert_eq!(m.notification.expect("row").state, "suppressed");
                }
                _ => panic!("expected Mutate variant"),
            }
        })
        .await;
    }
}
