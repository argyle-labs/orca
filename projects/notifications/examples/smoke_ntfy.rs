//! Live ntfy smoke test against an external server. Run with:
//!
//! ```sh
//! NTFY_BASE=http://10.10.10.6:8080 NTFY_TOPIC=orca-test \
//!   cargo run -p notifications --example smoke_ntfy
//! ```
//!
//! Verifies the full Event → NtfyBackend → ntfy::Client → HTTP path. Not a
//! unit test (it needs a live server); kept as a binary example so it can be
//! invoked from runbooks and CI's optional integration matrix.

use notifications::{Backend, Event, EventClass, Field, NtfyBackend, Severity};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("NTFY_BASE").unwrap_or_else(|_| "http://10.10.10.6:8080".into());
    let topic = std::env::var("NTFY_TOPIC").unwrap_or_else(|_| "orca-test".into());
    let token = std::env::var("NTFY_TOKEN").ok();

    let mut cfg = ntfy::Config::new(&base, &topic);
    if let Some(t) = token {
        cfg = cfg.with_token(t);
    }
    let backend = NtfyBackend::new("ntfy:primary", ntfy::Client::new(cfg));

    let event = Event::new(
        EventClass::Lifecycle,
        Severity::Info,
        "notifications crate live",
        "smoke:cli",
    )
    .with_body(
        "Markdown rendering, emoji tags, and click-through enabled.\n\n\
         Tap the title to open the topic in the ntfy web UI.",
    )
    .with_host("orca-dev")
    .with_click(format!("https://ntfy.scottkey.me/{topic}"));

    let event = Event {
        fields: vec![
            Field {
                key: "phase".into(),
                value: "ROADMAP §1.20".into(),
                inline: true,
            },
            Field {
                key: "backend".into(),
                value: backend.name().into(),
                inline: true,
            },
            Field {
                key: "topic".into(),
                value: topic.clone(),
                inline: false,
            },
            Field {
                key: "what changed".into(),
                value: "X-Markdown + X-Tags + X-Click now wired from Event".into(),
                inline: false,
            },
        ],
        ..event
    };

    let r = backend.emit(&event).await?;
    println!("emitted: backend={} id={}", r.backend, r.id);
    Ok(())
}
