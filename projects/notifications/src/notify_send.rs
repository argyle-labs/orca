//! `notify.send` tool. Relocated from `notifications/` so `notifications`
//! is pure plumbing (trait + dispatcher) and can be re-exported via
//! `notifications` without a cycle. See db→system for the
//! same shape.

use crate::{Event, EventClass, Severity, emit, registered_backend_names};

use anyhow::{Result, bail};
use contract::ToolCtx;
use derive::{orca_tool, plugin_struct};

#[plugin_struct(args, crate = ::macro_runtime)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifySendArgs {
    /// Event class — one of `heartbeat`, `drift`, `rotation`, `lifecycle`,
    /// `alert`, `approval`. Defaults to `alert`.
    #[arg(long)]
    pub class: Option<String>,
    /// Severity — one of `info`, `warn`, `error`, `critical`. Defaults to `info`.
    #[arg(long)]
    pub severity: Option<String>,
    /// Short title — rendered as the notification heading.
    #[arg(long)]
    pub title: String,
    /// Optional markdown-rendered body.
    #[arg(long)]
    pub body: Option<String>,
    /// Host this event is about (not necessarily this host).
    #[arg(long)]
    pub host: Option<String>,
    /// Emitter identifier, e.g. `reconciler:lxc`. Defaults to `notify.send`.
    #[arg(long)]
    pub source: Option<String>,
    /// Optional tap-through URL surfaced as the click target on backends
    /// that support one (ntfy `X-Click`).
    #[arg(long)]
    pub click: Option<String>,
}

#[plugin_struct(crate = ::macro_runtime)]
#[serde(rename_all = "camelCase")]
pub struct NotifySendBackendResult {
    pub backend: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[plugin_struct(crate = ::macro_runtime)]
#[serde(rename_all = "camelCase")]
pub struct NotifySendOutput {
    /// True when the global dispatcher was installed and ran. False when
    /// notifications are unconfigured on this host (no backends to send to).
    pub configured: bool,
    pub results: Vec<NotifySendBackendResult>,
}

fn parse_class(s: &str) -> Result<EventClass> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "heartbeat" => EventClass::Heartbeat,
        "drift" => EventClass::Drift,
        "rotation" => EventClass::Rotation,
        "lifecycle" => EventClass::Lifecycle,
        "alert" => EventClass::Alert,
        "approval" => EventClass::Approval,
        other => bail!("unknown event class `{other}`"),
    })
}

fn parse_severity_word(s: &str) -> Result<Severity> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "info" => Severity::Info,
        "warn" => Severity::Warn,
        "error" => Severity::Error,
        "critical" => Severity::Critical,
        other => bail!("unknown severity `{other}`"),
    })
}

/// Emit a notification event through this host's installed dispatcher. The
/// event is built from the supplied fields and fanned out per the configured
/// routing rules. When no dispatcher is installed, returns `configured=false`
/// with an empty result list — callers can treat that as a soft no-op.
#[orca_tool(domain = "notify", verb = "send", crate = ::macro_runtime)]
async fn notify_send(args: NotifySendArgs, _ctx: &ToolCtx) -> Result<NotifySendOutput> {
    let class = parse_class(args.class.as_deref().unwrap_or("alert"))?;
    let severity = parse_severity_word(args.severity.as_deref().unwrap_or("info"))?;
    let source = args.source.unwrap_or_else(|| "notify.send".to_string());
    let mut event = Event::new(class, severity, args.title, source);
    if let Some(b) = args.body {
        event = event.with_body(b);
    }
    if let Some(h) = args.host {
        event = event.with_host(h);
    }
    if let Some(c) = args.click {
        event = event.with_click(c);
    }

    let names = registered_backend_names();
    if names.is_empty() {
        return Ok(NotifySendOutput {
            configured: false,
            results: Vec::new(),
        });
    }
    let outcomes = emit(&event).await;
    Ok(NotifySendOutput {
        configured: true,
        results: outcomes
            .into_iter()
            .map(|o| NotifySendBackendResult {
                backend: o.backend,
                ok: o.result.is_ok(),
                error: o.result.err().map(|e| e.to_string()),
            })
            .collect(),
    })
}
