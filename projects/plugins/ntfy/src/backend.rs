//! `notifications::Backend` implementation for an ntfy endpoint. Each
//! registered ntfy endpoint (one db row) becomes one backend, named after the
//! endpoint's `name` column so routing rules can target it as `send = ["home"]`.

use plugin_toolkit::notify::{Backend, BackendError, Event, EventClass, MessageRef, Severity};
use plugin_toolkit::prelude::*;

use crate::{Client, Message, Priority};

/// One ntfy endpoint exposed as a notification backend.
pub struct NtfyBackend {
    name: String,
    client: Client,
}

impl NtfyBackend {
    pub fn new(name: impl Into<String>, client: Client) -> Self {
        Self {
            name: name.into(),
            client,
        }
    }
}

fn severity_emoji(s: Severity) -> &'static str {
    match s {
        Severity::Info => "white_check_mark",
        Severity::Warn => "warning",
        Severity::Error => "rotating_light",
        Severity::Critical => "fire",
    }
}

fn class_emoji(c: EventClass) -> &'static str {
    match c {
        EventClass::Heartbeat => "heartbeat",
        EventClass::Drift => "compass",
        EventClass::Rotation => "arrows_counterclockwise",
        EventClass::Lifecycle => "package",
        EventClass::Alert => "bell",
        EventClass::Approval => "raised_hand",
    }
}

#[async_trait]
impl Backend for NtfyBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn emit(&self, event: &Event) -> Result<MessageRef, BackendError> {
        let priority = match event.severity {
            Severity::Info => Priority::Default,
            Severity::Warn => Priority::High,
            Severity::Error | Severity::Critical => Priority::Urgent,
        };
        let tags = vec![severity_emoji(event.severity), class_emoji(event.class)];

        let mut body = String::new();
        if let Some(host) = &event.host {
            body.push_str(&format!("**host:** `{host}`\n"));
        }
        if !event.body.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&event.body);
            body.push('\n');
        }
        if !event.fields.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            for f in &event.fields {
                body.push_str(&format!("- **{}:** {}\n", f.key, f.value));
            }
        }
        body.push_str(&format!("\n_via {}_\n", event.source));

        let result = self
            .client
            .send(Message {
                message: &body,
                title: Some(&event.title),
                priority: Some(priority),
                tags,
                click: event.click.as_deref(),
                markdown: true,
                ..Default::default()
            })
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if !result.ok {
            return Err(BackendError::Transport(format!(
                "ntfy returned status {}",
                result.status
            )));
        }
        Ok(MessageRef::new(
            self.name.clone(),
            format!("ntfy:{}", result.status),
        ))
    }
}
