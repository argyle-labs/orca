//! DEV-ONLY terminal client for `/api/subscribe/fleet-update`.
//!
//! Connects to the endpoint as a REAL WebSocket client (not an in-process
//! mock) and renders incoming `FleetUpdateEvent` frames live in a table — this
//! is what proves the wire format and the endpoint actually work, before any
//! real frontend touches them. Never wired into the public `orca` command
//! tree as a first-class feature; reached only via the hidden
//! `orca dev subscribe-tui` subcommand.
//!
//! Terminal setup (alternate screen, raw mode, restore-on-every-path) mirrors
//! `system::logs_tui`; the model here is a one-row-per-host table instead of
//! a scrolling, filterable log.

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Row as TableRow, Table},
};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

/// Redraw/poll cadence.
const TICK: Duration = Duration::from_millis(120);

const MIN_COLS: u16 = 40;
const MIN_ROWS: u16 = 8;

fn usable_size(cols: u16, rows: u16) -> bool {
    cols >= MIN_COLS && rows >= MIN_ROWS
}

/// One host's latest narrated outcome, keyed by host name in [`App::rows`].
struct HostRow {
    status: String,
    version: String,
}

enum Incoming {
    /// One WS frame, exactly as the daemon sent it.
    Frame(String),
    /// The socket closed or errored — terminal, no more frames are coming.
    Disconnected(String),
}

struct App {
    rows: BTreeMap<String, HostRow>,
    connection: String,
    gap: bool,
    last_note: Option<String>,
    should_quit: bool,
}

/// Fold one incoming payload into the table/status state. Pure so the
/// "what does this frame mean" decision is testable without a socket.
fn apply_payload(app: &mut App, payload: &str) {
    #[allow(clippy::disallowed_types)]
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        app.last_note = Some(payload.to_string());
        return;
    };
    if value.get("gap").and_then(|v| v.as_bool()) == Some(true) {
        app.gap = true;
        let dropped = value.get("dropped").and_then(|v| v.as_u64()).unwrap_or(0);
        app.last_note = Some(format!(
            "GAP: {dropped} event(s) dropped — view may be stale"
        ));
        return;
    }
    let Some(host) = value.get("host").and_then(|v| v.as_str()) else {
        // Not a system row — a daemon-phase note (or an error) narrated verbatim.
        app.last_note = value
            .as_str()
            .map(str::to_string)
            .or(Some(payload.to_string()));
        return;
    };
    let field = |k: &str| value.get(k).and_then(|v| v.as_str());
    let status = field("status").unwrap_or("unknown").to_string();
    let version = field("applied")
        .or_else(|| field("target"))
        .or_else(|| field("current"))
        .unwrap_or("?")
        .to_string();
    app.rows
        .insert(host.to_string(), HostRow { status, version });
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    let header = TableRow::new(["Host", "Status", "Version"]);
    let rows = app
        .rows
        .iter()
        .map(|(host, row)| TableRow::new([host.clone(), row.status.clone(), row.version.clone()]));
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("fleet-update (dev subscribe TUI) — q to quit"),
    );
    f.render_widget(table, chunks[0]);

    let gap = if app.gap { " [GAP DETECTED]" } else { "" };
    let note = app.last_note.as_deref().unwrap_or("");
    let status_style = if app.gap {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let status = Paragraph::new(Line::styled(
        format!("{}{gap} — {note}", app.connection),
        status_style,
    ));
    f.render_widget(status, chunks[1]);
}

async fn read_frames(url: String, token: String, tx: mpsc::UnboundedSender<Incoming>) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            _ = tx.send(Incoming::Disconnected(format!("bad url: {e}")));
            return;
        }
    };
    let header = match tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
        "Bearer {token}"
    )) {
        Ok(h) => h,
        Err(e) => {
            _ = tx.send(Incoming::Disconnected(format!("bad token: {e}")));
            return;
        }
    };
    req.headers_mut().insert("authorization", header);

    let (ws, _) = match tokio_tungstenite::connect_async(req).await {
        Ok(v) => v,
        Err(e) => {
            _ = tx.send(Incoming::Disconnected(format!("connect failed: {e}")));
            return;
        }
    };
    let (_, mut read) = ws.split();
    while let Some(msg) = read.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Text(t)) => {
                if tx.send(Incoming::Frame(t.to_string())).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                _ = tx.send(Incoming::Disconnected(format!("read error: {e}")));
                return;
            }
        }
    }
    _ = tx.send(Incoming::Disconnected("socket closed".to_string()));
}

/// Run the dev TUI against `url` (e.g. `ws://127.0.0.1:12000/api/subscribe/fleet-update`)
/// with `token` as the bearer presented to `require_auth`.
pub async fn run(url: String, token: String) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    if !usable_size(cols, rows) {
        anyhow::bail!(
            "terminal is {cols}x{rows}; the subscribe TUI needs at least {MIN_COLS}x{MIN_ROWS}."
        );
    }

    let mut app = App {
        rows: BTreeMap::new(),
        connection: format!("connecting to {url}…"),
        gap: false,
        last_note: None,
        should_quit: false,
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(read_frames(url.clone(), token, tx));
    app.connection = format!("connected to {url}");

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend).context("init terminal")?;

    let result = event_loop(&mut term, &mut app, &mut rx).await;

    // Restore the terminal on EVERY path, including the error one.
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        term.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    term.show_cursor()?;
    result
}

async fn event_loop(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<Incoming>,
) -> Result<()> {
    loop {
        while let Ok(incoming) = rx.try_recv() {
            match incoming {
                Incoming::Frame(payload) => apply_payload(app, &payload),
                Incoming::Disconnected(why) => {
                    app.connection = format!("disconnected: {why}");
                }
            }
        }

        term.draw(|f| draw(f, app))?;

        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            app.should_quit = true;
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_app() -> App {
        App {
            rows: BTreeMap::new(),
            connection: "test".into(),
            gap: false,
            last_note: None,
            should_quit: false,
        }
    }

    #[test]
    fn a_system_row_payload_is_tracked_by_host() {
        let mut app = fresh_app();
        apply_payload(
            &mut app,
            r#"{"host":"thor","status":"updated","applied":"0.2.1"}"#,
        );
        let row = app.rows.get("thor").expect("row present");
        assert_eq!(row.status, "updated");
        assert_eq!(row.version, "0.2.1");
    }

    #[test]
    fn a_later_row_for_the_same_host_overwrites_in_place() {
        let mut app = fresh_app();
        apply_payload(
            &mut app,
            r#"{"host":"thor","status":"update_available","current":"0.2.0"}"#,
        );
        apply_payload(
            &mut app,
            r#"{"host":"thor","status":"updated","applied":"0.2.1"}"#,
        );
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows["thor"].status, "updated");
    }

    #[test]
    fn a_gap_notice_sets_the_gap_flag_and_names_the_count() {
        let mut app = fresh_app();
        apply_payload(&mut app, r#"{"gap":true,"dropped":7}"#);
        assert!(app.gap);
        assert!(app.last_note.as_deref().unwrap().contains('7'));
    }

    #[test]
    fn a_plain_note_string_updates_the_status_line_not_the_table() {
        let mut app = fresh_app();
        apply_payload(&mut app, r#""thor: applied 0.2.1, health-gating…""#);
        assert!(app.rows.is_empty());
        assert!(app.last_note.is_some());
    }

    #[test]
    fn malformed_json_is_shown_verbatim_rather_than_panicking() {
        let mut app = fresh_app();
        apply_payload(&mut app, "not json at all");
        assert_eq!(app.last_note.as_deref(), Some("not json at all"));
    }

    #[test]
    fn usable_size_rejects_a_zero_sized_terminal() {
        assert!(!usable_size(0, 0));
        assert!(usable_size(MIN_COLS, MIN_ROWS));
    }
}
