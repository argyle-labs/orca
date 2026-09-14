//! In-tree SMTP/email [`Backend`]. Renders an [`Event`] to a plain-text email
//! and relays it via async lettre over rustls (no openssl — musl-safe).
//!
//! Email can't render interactive [`Action`](crate::Action) buttons, so actions
//! degrade to `- {label}` lines and `click` becomes a trailing `Open: {url}`
//! line, per the crate's rendering guidance.

use std::sync::Arc;

use db::pool::Db;
use derive::orca_async;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::{Backend, BackendError, Event, MessageRef, Severity};

/// Resolved SMTP backend config. Built once at startup from settings; recipients
/// come from a configured list because the identity directory has no email field.
pub struct SmtpBackend {
    host: String,
    port: u16,
    from: String,
    recipients: Vec<String>,
    username: Option<String>,
    password: Option<String>,
    starttls: bool,
    min_severity: Severity,
}

#[orca_async]
impl Backend for SmtpBackend {
    fn name(&self) -> &str {
        "smtp"
    }

    async fn emit(&self, event: &Event) -> Result<MessageRef, BackendError> {
        // Spam guard: SMTP only mails Warn+ by default, without touching global
        // routing. Below the floor is a no-op success, not an error.
        if !passes_floor(event.severity, self.min_severity) {
            return Ok(MessageRef::new("smtp", "skipped"));
        }

        let from: Mailbox = self
            .from
            .parse()
            .map_err(|e| BackendError::Transport(format!("invalid from address: {e}")))?;
        let subject = render_subject(event);
        let body = render_body(event);

        let mut builder = Message::builder().from(from).subject(subject);
        for rcpt in &self.recipients {
            let to: Mailbox = rcpt
                .parse()
                .map_err(|e| BackendError::Transport(format!("invalid recipient address: {e}")))?;
            builder = builder.to(to);
        }
        let message = builder
            .body(body)
            .map_err(|e| BackendError::Transport(format!("build message: {e}")))?;

        // Build the transport per-send: correctness over connection reuse.
        let mut transport = if self.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)
        }
        .map_err(|e| BackendError::Transport(format!("smtp relay setup: {e}")))?
        .port(self.port);

        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            transport = transport.credentials(Credentials::new(user.clone(), pass.clone()));
        }

        let response = transport
            .build()
            .send(message)
            .await
            .map_err(|e| BackendError::Transport(format!("smtp send: {e}")))?;

        // The final line of the SMTP response often carries the queued id; fall
        // back to a fixed marker so the MessageRef is always populated.
        let id = response
            .message()
            .last()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sent".to_string());
        Ok(MessageRef::new("smtp", id))
    }
}

/// True when `sev` clears the `floor` (the min-severity gate).
fn passes_floor(sev: Severity, floor: Severity) -> bool {
    sev >= floor
}

/// Single-line subject: `[SEVERITY] title`, with newlines stripped.
pub fn render_subject(event: &Event) -> String {
    let word = match event.severity {
        Severity::Info => "INFO",
        Severity::Warn => "WARN",
        Severity::Error => "ERROR",
        Severity::Critical => "CRITICAL",
    };
    let title = event.title.replace(['\r', '\n'], " ");
    format!("[{word}] {title}")
}

/// Plain-text body: title, blank line, body; then Host/Source lines when set;
/// then each field as `key: value`; actions degrade to `- label` lines; a set
/// `click` becomes a trailing `Open: url` line.
pub fn render_body(event: &Event) -> String {
    let mut out = String::new();
    out.push_str(&event.title);
    out.push_str("\n\n");
    if !event.body.is_empty() {
        out.push_str(&event.body);
        out.push('\n');
    }
    if let Some(host) = &event.host {
        out.push_str(&format!("Host: {host}\n"));
    }
    out.push_str(&format!("Source: {}\n", event.source));
    for f in &event.fields {
        out.push_str(&format!("{}: {}\n", f.key, f.value));
    }
    for a in &event.actions {
        out.push_str(&format!("- {}\n", a.label));
    }
    if let Some(click) = &event.click {
        out.push_str(&format!("Open: {click}\n"));
    }
    out
}

/// Split a recipients setting on commas/whitespace, trimming and dropping empties.
pub fn parse_recipients(raw: &str) -> Vec<String> {
    raw.split([',', ' ', '\t', '\n', '\r'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Parse a severity name (case-insensitive), defaulting to `Warn` on unknown.
pub fn parse_severity(raw: &str) -> Severity {
    match raw.trim().to_ascii_lowercase().as_str() {
        "info" => Severity::Info,
        "error" => Severity::Error,
        "critical" => Severity::Critical,
        _ => Severity::Warn,
    }
}

/// Read SMTP settings and register an [`SmtpBackend`] iff host+from+recipients
/// are all present. Best-effort: missing config logs and returns without
/// registering; it never panics or fails daemon startup.
pub fn register_from_settings() {
    let loaded = Db::process().read(|c| {
        let host = db::settings::get(c, "notify.smtp.host")?;
        let from = db::settings::get(c, "notify.smtp.from")?;
        let recipients = db::settings::get(c, "notify.smtp.recipients")?;
        let port = db::settings::get(c, "notify.smtp.port")?;
        let username = db::settings::get(c, "notify.smtp.username")?;
        let password = db::settings::secret_get(c, "notify.smtp.password")?;
        let starttls = db::settings::get(c, "notify.smtp.starttls")?;
        let min_severity = db::settings::get(c, "notify.smtp.min_severity")?;
        Ok((
            host,
            from,
            recipients,
            port,
            username,
            password,
            starttls,
            min_severity,
        ))
    });

    let (host, from, recipients, port, username, password, starttls, min_severity) = match loaded {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("smtp backend: settings read failed, not registering: {e:#}");
            return;
        }
    };

    let (host, from, recipients_raw) = match (host, from, recipients) {
        (Some(h), Some(f), Some(r)) if !h.trim().is_empty() && !f.trim().is_empty() => (h, f, r),
        _ => {
            tracing::info!("smtp backend: host/from/recipients not fully configured, skipping");
            return;
        }
    };
    let recipients = parse_recipients(&recipients_raw);
    if recipients.is_empty() {
        tracing::info!("smtp backend: no valid recipients configured, skipping");
        return;
    }

    let port = port
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(587);
    let starttls = starttls
        .map(|s| !matches!(s.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"))
        .unwrap_or(true);
    let min_severity = min_severity
        .as_deref()
        .map(parse_severity)
        .unwrap_or(Severity::Warn);
    let username = username.filter(|s| !s.trim().is_empty());

    crate::register_backend(Arc::new(SmtpBackend {
        host,
        port,
        from,
        recipients,
        username,
        password,
        starttls,
        min_severity,
    }));
    tracing::info!("smtp notification backend registered");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, ActionStyle, EventClass, Field};

    fn base_event() -> Event {
        Event::new(EventClass::Drift, Severity::Warn, "disk filling up", "detector:disk")
    }

    #[test]
    fn subject_has_severity_and_title_single_line() {
        let mut e = base_event();
        e.title = "line one\nline two".into();
        let s = render_subject(&e);
        assert!(s.contains("WARN"));
        assert!(s.contains("line one"));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn body_renders_content_fields_actions_and_click() {
        let mut e = base_event()
            .with_body("root fs at 92%")
            .with_host("node-a")
            .with_click("https://example.test/change/42");
        e.fields.push(Field {
            key: "mount".into(),
            value: "/".into(),
            inline: true,
        });
        e.actions.push(Action {
            id: "approve".into(),
            label: "Approve cleanup".into(),
            style: ActionStyle::Primary,
            correlation: "42".into(),
        });
        let b = render_body(&e);
        assert!(b.contains("disk filling up"));
        assert!(b.contains("root fs at 92%"));
        assert!(b.contains("Host: node-a"));
        assert!(b.contains("Source: detector:disk"));
        assert!(b.contains("mount: /"));
        assert!(b.contains("- Approve cleanup"));
        assert!(b.contains("Open: https://example.test/change/42"));
    }

    #[test]
    fn body_omits_host_line_when_absent() {
        let b = render_body(&base_event());
        assert!(!b.contains("Host:"));
        assert!(b.contains("Source: detector:disk"));
    }

    #[test]
    fn parse_recipients_splits_on_commas_and_whitespace() {
        let r = parse_recipients("a@x.test, b@y.test  c@z.test");
        assert_eq!(r, vec!["a@x.test", "b@y.test", "c@z.test"]);
        assert!(parse_recipients("   \n  ").is_empty());
        assert!(parse_recipients("").is_empty());
    }

    #[test]
    fn parse_severity_maps_known_and_defaults_to_warn() {
        assert_eq!(parse_severity("Critical"), Severity::Critical);
        assert_eq!(parse_severity("warn"), Severity::Warn);
        assert_eq!(parse_severity("INFO"), Severity::Info);
        assert_eq!(parse_severity("nonsense"), Severity::Warn);
    }

    #[test]
    fn severity_floor_gate() {
        assert!(!passes_floor(Severity::Info, Severity::Warn));
        assert!(passes_floor(Severity::Warn, Severity::Warn));
        assert!(passes_floor(Severity::Critical, Severity::Warn));
    }
}
