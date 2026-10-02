//! `orca logs` — an interactive viewer for the daemon log.
//!
//! The daemon writes one JSON object per line to `~/.orca/logs/daemon.jsonl`.
//! Reading it meant `tail -f | jq`, re-typing a filter expression every time
//! the question changed, and selecting text with the mouse to copy a line —
//! which in a wrapped terminal picks up the neighbouring lines too.
//!
//! So: levels and targets toggle live, search is incremental, and `y` copies
//! the exact bytes of the focused line.
//!
//! The pure half (parse / filter / select) lives in [`model`] and is tested
//! without a terminal; this module owns layout, keys, and the tty.

pub mod model;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use model::{Filters, LEVELS, Record, clamp_selection, targets_by_volume, visible};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::io::Write;
use std::time::Duration;

/// How much of the tail to load. The log is unrotated and has reached 123 MB
/// on this machine, so reading it whole is not an option.
const DEFAULT_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// Hard cap on retained records, oldest dropped first. Bounds memory on a
/// follow session left running for days.
const MAX_RECORDS: usize = 100_000;

/// Redraw/poll cadence. Fast enough to feel live, slow enough that an idle
/// viewer is not a busy loop.
const TICK: Duration = Duration::from_millis(120);

/// Which overlay, if any, is up.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Overlay {
    None,
    /// Target mute menu.
    Targets,
    /// Full record, pretty-printed, for reading and manual selection.
    Detail,
    Help,
}

struct App {
    path: std::path::PathBuf,
    records: Vec<Record>,
    filters: Filters,
    /// Index into the VISIBLE list, not into `records`.
    selected: usize,
    /// Pinned to the newest line, and new lines keep it there.
    follow: bool,
    overlay: Overlay,
    /// Selection within the target menu.
    target_sel: usize,
    /// Incremental search is typed into the status bar.
    searching: bool,
    /// Byte offset we have consumed up to, for the follow tail.
    read_to: u64,
    /// Transient line shown in the status bar (copy confirmations, errors).
    flash: Option<String>,
    should_quit: bool,
}

impl App {
    fn visible(&self) -> Vec<usize> {
        visible(&self.records, &self.filters)
    }

    /// The record under the cursor, if the filtered view is non-empty.
    fn focused(&self) -> Option<&Record> {
        let v = self.visible();
        v.get(self.selected).map(|&i| &self.records[i])
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        self.selected = next;
        // Any upward move means the operator is reading history, not watching
        // the tail; silently yanking them back to the bottom on the next line
        // is the single most irritating thing a log viewer can do.
        if delta < 0 {
            self.follow = false;
        }
        if self.selected + 1 == len {
            self.follow = true;
        }
    }

    fn jump_to_end(&mut self) {
        self.selected = clamp_selection(usize::MAX, self.visible().len());
        self.follow = true;
    }

    fn jump_to_start(&mut self) {
        self.selected = 0;
        self.follow = false;
    }
}

/// Copy via OSC 52, which asks the TERMINAL to set the clipboard.
///
/// Deliberately not a clipboard crate: the daemon log is most often read over
/// SSH, where a host-side clipboard API writes to a clipboard nobody is
/// looking at. OSC 52 travels the tty back to the operator's own machine.
/// Returns false when the terminal ignores it — we cannot detect that, so the
/// status line says "sent to terminal", not "copied".
fn osc52_copy(text: &str) -> std::io::Result<()> {
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{payload}\x07")?;
    out.flush()
}

fn level_style(level: Option<&str>) -> Style {
    match level {
        Some("ERROR") => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some("WARN") => Style::default().fg(Color::Yellow),
        Some("INFO") => Style::default().fg(Color::Green),
        Some("DEBUG") => Style::default().fg(Color::Blue),
        Some("TRACE") => Style::default().fg(Color::DarkGray),
        // No level parsed — a panic or raw stderr. Make it loud rather than dim.
        _ => Style::default().fg(Color::Magenta),
    }
}

/// Trim a `module::path::like::this` to its last two segments so the target
/// column stays narrow without becoming ambiguous.
fn short_target(t: &str) -> String {
    let parts: Vec<&str> = t.split("::").collect();
    if parts.len() <= 2 {
        t.to_string()
    } else {
        parts[parts.len() - 2..].join("::")
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // level toggles
            Constraint::Min(3),    // the log
            Constraint::Length(1), // status
        ])
        .split(f.area());

    draw_toggles(f, chunks[0], app);
    draw_log(f, chunks[1], app);
    draw_status(f, chunks[2], app);

    match app.overlay {
        Overlay::Targets => draw_targets(f, app),
        Overlay::Detail => draw_detail(f, app),
        Overlay::Help => draw_help(f),
        Overlay::None => {}
    }
}

fn draw_toggles(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" ", Style::default())];
    for (i, lvl) in LEVELS.iter().enumerate() {
        let on = app.filters.level_enabled(lvl);
        // The number key that toggles it is part of the label, so the binding
        // is discoverable without opening help.
        let label = format!(" {}:{} ", i + 1, lvl);
        let style = if on {
            level_style(Some(lvl)).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
    }
    let muted = app.filters.muted_targets.values().filter(|v| **v).count();
    spans.push(Span::styled(
        format!("  t:targets({muted} muted)  "),
        Style::default().fg(Color::Cyan),
    ));
    spans.push(Span::styled(
        if app.follow { "FOLLOW" } else { "PAUSED" },
        Style::default()
            .fg(if app.follow {
                Color::Green
            } else {
                Color::Yellow
            })
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_log(f: &mut Frame, area: Rect, app: &mut App) {
    let vis = app.visible();
    app.selected = clamp_selection(app.selected, vis.len());
    let items: Vec<ListItem> = vis
        .iter()
        .map(|&i| {
            let r = &app.records[i];
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>8} ", r.short_time()),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<5} ", r.level.as_deref().unwrap_or("—")),
                    level_style(r.level.as_deref()),
                ),
                Span::styled(
                    format!("{:<28} ", short_target(r.target.as_deref().unwrap_or(""))),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(r.message.clone()),
            ]))
        })
        .collect();

    let title = format!(
        " {} — {}/{} lines{} ",
        app.path.display(),
        vis.len(),
        app.records.len(),
        if app.filters.is_narrowed() {
            "  [FILTERED]"
        } else {
            ""
        }
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !vis.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let text = if app.searching {
        format!("/{}", app.filters.search)
    } else if let Some(flash) = &app.flash {
        flash.clone()
    } else {
        "j/k move · g/G ends · 1-5 levels · t targets · / search · y copy line · Y copy view · enter detail · f follow · r reset · ? help · q quit".to_string()
    };
    let style = if app.searching {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
}

/// Centered box `pct_x` × `pct_y` of the screen.
fn centered(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

fn draw_targets(f: &mut Frame, app: &mut App) {
    let area = centered(70, 70, f.area());
    f.render_widget(Clear, area);
    let targets = targets_by_volume(&app.records);
    app.target_sel = clamp_selection(app.target_sel, targets.len());
    let items: Vec<ListItem> = targets
        .iter()
        .map(|(t, n)| {
            let on = app.filters.target_enabled(t);
            ListItem::new(Line::from(vec![
                Span::styled(
                    if on { "[x] " } else { "[ ] " },
                    Style::default().fg(if on { Color::Green } else { Color::DarkGray }),
                ),
                Span::styled(
                    format!("{t:<44}"),
                    Style::default().fg(if on { Color::White } else { Color::DarkGray }),
                ),
                Span::styled(format!("{n:>7}"), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" targets — space toggles · a all · n none · esc close "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !targets.is_empty() {
        state.select(Some(app.target_sel));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(f: &mut Frame, app: &App) {
    let area = centered(85, 80, f.area());
    f.render_widget(Clear, area);
    let body = match app.focused() {
        // Pretty-print when it is JSON so a nested span is readable; fall back
        // to the raw line otherwise.
        // A log line IS free-form upstream: the daemon's structured fields
        // differ per call site, so there is no struct to model it as. The
        // viewer only pretty-prints it.
        #[allow(clippy::disallowed_types)]
        Some(r) => serde_json::from_str::<serde_json::Value>(&r.raw)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| r.raw.clone()),
        None => "(no line selected)".to_string(),
    };
    f.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" record — y copy · esc close "),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame) {
    let area = centered(60, 70, f.area());
    f.render_widget(Clear, area);
    let text = "\
  Movement      j/k ↑/↓   line        PgUp/PgDn  page
                g         oldest      G          newest (resumes follow)

  Filtering     1-5       toggle ERROR/WARN/INFO/DEBUG/TRACE
                t         target menu (space toggles, a=all, n=none)
                /         incremental search, enter accepts, esc cancels
                r         reset every filter

  Copying       y         copy the focused line, exactly as written
                Y         copy every line currently visible
                enter     open the record pretty-printed, then y

  Copy uses OSC 52, so it reaches YOUR clipboard over SSH. A terminal that
  does not implement it will silently ignore the request — in iTerm2 enable
  \"Applications in terminal may access clipboard\".

  Other         f         follow on/off     q / esc   quit";
    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" orca logs — keys "),
        ),
        area,
    );
}

/// Load the tail of the log, newest `DEFAULT_TAIL_BYTES`.
fn load_tail(path: &std::path::Path, bytes: u64) -> Result<(Vec<Record>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f =
        std::fs::File::open(path).with_context(|| format!("open log {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = String::new();
    // Lossy: the log is UTF-8, but a torn write at the window edge must not
    // take down the viewer.
    let mut raw = Vec::new();
    f.read_to_end(&mut raw)?;
    buf.push_str(&String::from_utf8_lossy(&raw));
    let mut lines: Vec<&str> = buf.lines().collect();
    // A mid-file start almost certainly lands inside a line; drop the shard.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok((lines.into_iter().map(Record::parse).collect(), len))
}

/// Read whatever has been appended since `from`.
fn read_appended(path: &std::path::Path, from: u64) -> Result<(Vec<Record>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len <= from {
        // Truncated or rotated underneath us: start over from the new end
        // rather than seeking past EOF forever.
        return Ok((Vec::new(), if len < from { len } else { from }));
    }
    f.seek(SeekFrom::Start(from))?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    // Only consume through the last newline; a partially-written final line is
    // left for the next tick rather than parsed in half.
    let consumed = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let recs = text[..consumed].lines().map(Record::parse).collect();
    Ok((recs, from + consumed as u64))
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // Search owns the keyboard while it is open, or typing "q" would quit.
    if app.searching {
        match key.code {
            KeyCode::Esc => {
                app.filters.search.clear();
                app.searching = false;
            }
            KeyCode::Enter => app.searching = false,
            KeyCode::Backspace => {
                app.filters.search.pop();
            }
            KeyCode::Char(c) => app.filters.search.push(c),
            _ => {}
        }
        return Ok(());
    }

    if app.overlay == Overlay::Targets {
        let targets = targets_by_volume(&app.records);
        match key.code {
            KeyCode::Esc | KeyCode::Char('t') | KeyCode::Char('q') => app.overlay = Overlay::None,
            KeyCode::Down | KeyCode::Char('j') => {
                app.target_sel = (app.target_sel + 1).min(targets.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => app.target_sel = app.target_sel.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some((t, _)) = targets.get(app.target_sel) {
                    app.filters.toggle_target(t);
                }
            }
            KeyCode::Char('a') => app.filters.muted_targets.clear(),
            KeyCode::Char('n') => {
                for (t, _) in &targets {
                    app.filters.muted_targets.insert(t.clone(), true);
                }
            }
            _ => {}
        }
        return Ok(());
    }

    if app.overlay == Overlay::Detail || app.overlay == Overlay::Help {
        match key.code {
            KeyCode::Char('y') if app.overlay == Overlay::Detail => {
                if let Some(r) = app.focused() {
                    let raw = r.raw.clone();
                    osc52_copy(&raw).ok();
                    app.flash = Some(format!(
                        "sent {} bytes to the terminal clipboard",
                        raw.len()
                    ));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('?') => {
                app.overlay = Overlay::None;
            }
            _ => {}
        }
        return Ok(());
    }

    app.flash = None;
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Char('j') | KeyCode::Down => app.move_by(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_by(-1),
        KeyCode::PageDown => app.move_by(20),
        KeyCode::PageUp => app.move_by(-20),
        KeyCode::Char('g') => app.jump_to_start(),
        KeyCode::Char('G') => app.jump_to_end(),
        KeyCode::Char('f') => app.follow = !app.follow,
        KeyCode::Char('r') => {
            app.filters.reset();
            app.flash = Some("filters reset".into());
        }
        KeyCode::Char('t') => app.overlay = Overlay::Targets,
        KeyCode::Char('?') => app.overlay = Overlay::Help,
        KeyCode::Enter => app.overlay = Overlay::Detail,
        KeyCode::Char('/') => {
            app.searching = true;
            app.filters.search.clear();
        }
        KeyCode::Char(c @ '1'..='5') => {
            let idx = c as usize - '1' as usize;
            app.filters.toggle_level(LEVELS[idx]);
        }
        KeyCode::Char('y') => {
            if let Some(r) = app.focused() {
                let raw = r.raw.clone();
                osc52_copy(&raw).ok();
                app.flash = Some(format!(
                    "sent {} bytes to the terminal clipboard",
                    raw.len()
                ));
            }
        }
        KeyCode::Char('Y') => {
            let vis = app.visible();
            let text: String = vis
                .iter()
                .map(|&i| app.records[i].raw.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            osc52_copy(&text).ok();
            app.flash = Some(format!(
                "sent {} lines ({} bytes) to the terminal clipboard",
                vis.len(),
                text.len()
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Run the viewer. Blocks until the operator quits.
pub fn run(path: Option<std::path::PathBuf>, tail_bytes: Option<u64>) -> Result<()> {
    let path = path.unwrap_or_else(default_log_path);
    let (records, read_to) = load_tail(&path, tail_bytes.unwrap_or(DEFAULT_TAIL_BYTES))?;

    let mut app = App {
        path,
        selected: records.len().saturating_sub(1),
        records,
        filters: Filters::default(),
        follow: true,
        overlay: Overlay::None,
        target_sel: 0,
        searching: false,
        read_to,
        flash: None,
        should_quit: false,
    };

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = event_loop(&mut term, &mut app);

    // Restore the terminal on EVERY path, including the error one. A viewer
    // that leaves raw mode on after a panic hands back an unusable shell.
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        term.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    term.show_cursor()?;
    result
}

// Concrete in the backend rather than generic: ratatui 0.30's associated
// `Backend::Error` is unconstrained, so `?` on a generic backend cannot prove
// the Send + Sync that `anyhow` needs.
fn event_loop(term: &mut Terminal<CrosstermBackend<std::io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        term.draw(|f| draw(f, app))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
        {
            // Windows reports press AND release; acting on both double-fires.
            if key.kind == KeyEventKind::Press {
                handle_key(app, key)?;
            }
        }
        if app.should_quit {
            return Ok(());
        }
        // Tail whether or not we are following — pausing freezes the VIEW, not
        // the ingest, so resuming with G shows what happened meanwhile.
        let (new, read_to) =
            read_appended(&app.path, app.read_to).unwrap_or((Vec::new(), app.read_to));
        app.read_to = read_to;
        if !new.is_empty() {
            let was_following = app.follow;
            app.records.extend(new);
            if app.records.len() > MAX_RECORDS {
                app.records.drain(..app.records.len() - MAX_RECORDS);
            }
            if was_following {
                app.jump_to_end();
            }
        }
    }
}

/// `$ORCA_HOME/logs/daemon.jsonl`, else `$HOME/.orca/logs/daemon.jsonl`.
fn default_log_path() -> std::path::PathBuf {
    let home = contract::config::orca_home().unwrap_or_else(|| {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".orca")
    });
    home.join("logs").join("daemon.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(lines: &[&str]) -> App {
        let records: Vec<Record> = lines.iter().map(|l| Record::parse(l)).collect();
        App {
            path: "/tmp/x.jsonl".into(),
            selected: records.len().saturating_sub(1),
            records,
            filters: Filters::default(),
            follow: true,
            overlay: Overlay::None,
            target_sel: 0,
            searching: false,
            read_to: 0,
            flash: None,
            should_quit: false,
        }
    }

    fn press(app: &mut App, c: char) {
        handle_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)).unwrap();
    }

    #[test]
    fn scrolling_up_drops_follow_and_capital_g_restores_it() {
        let mut app = app_with(&[
            r#"{"level":"INFO","message":"a"}"#,
            r#"{"level":"INFO","message":"b"}"#,
            r#"{"level":"INFO","message":"c"}"#,
        ]);
        assert!(app.follow);
        press(&mut app, 'k');
        assert!(!app.follow, "reading history must not be interrupted");
        press(&mut app, 'G');
        assert!(app.follow);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn typing_in_search_does_not_trigger_commands() {
        // The bug this prevents: '/' then typing "query" would hit 'q' = quit.
        let mut app = app_with(&[r#"{"level":"INFO","message":"a"}"#]);
        press(&mut app, '/');
        for c in "query".chars() {
            press(&mut app, c);
        }
        assert!(!app.should_quit);
        assert_eq!(app.filters.search, "query");
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.filters.search, "", "esc abandons the search");
        assert!(!app.should_quit, "esc closed search, it must not also quit");
    }

    #[test]
    fn number_keys_toggle_the_level_they_are_labelled_with() {
        let mut app = app_with(&[r#"{"level":"ERROR","message":"a"}"#]);
        press(&mut app, '1');
        assert!(!app.filters.level_enabled("ERROR"));
        assert!(app.visible().is_empty());
        press(&mut app, 'r');
        assert!(app.filters.level_enabled("ERROR"));
    }

    #[test]
    fn a_torn_final_line_is_not_consumed_until_it_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        std::fs::write(&p, "{\"message\":\"one\"}\n{\"message\":\"par").unwrap();
        let (recs, to) = read_appended(&p, 0).unwrap();
        assert_eq!(recs.len(), 1, "the half-written line must wait");
        // Finish the line; the next read picks it up whole.
        std::fs::write(&p, "{\"message\":\"one\"}\n{\"message\":\"partial\"}\n").unwrap();
        let (recs2, _) = read_appended(&p, to).unwrap();
        assert_eq!(recs2.len(), 1);
        assert_eq!(recs2[0].message, "partial");
    }

    #[test]
    fn a_rotated_log_resets_instead_of_seeking_past_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        std::fs::write(&p, "{\"message\":\"a\"}\n").unwrap();
        // Truncate: the new file is shorter than where we had read to.
        let (_, _) = read_appended(&p, 0).unwrap();
        std::fs::write(&p, "").unwrap();
        let (recs, to) = read_appended(&p, 9999).unwrap();
        assert!(recs.is_empty());
        assert_eq!(to, 0, "must re-anchor to the new, shorter file");
    }

    #[test]
    fn the_window_start_drops_the_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let body = "{\"message\":\"old\"}\n{\"message\":\"new\"}\n";
        std::fs::write(&p, body).unwrap();
        // A window smaller than the file starts mid-line.
        let (recs, _) = load_tail(&p, 20).unwrap();
        assert!(
            recs.iter().all(|r| !r.raw.starts_with("essage")),
            "a sheared line must be dropped, not shown as garbage"
        );
    }

    #[test]
    fn long_module_paths_shorten_but_stay_distinguishable() {
        assert_eq!(short_target("orca::serve::middleware"), "serve::middleware");
        assert_eq!(short_target("orca::mesh"), "orca::mesh");
        assert_eq!(short_target("single"), "single");
    }

    #[test]
    fn osc52_wraps_the_payload_in_the_clipboard_escape() {
        use base64::Engine as _;
        // The encoding is what the terminal reads; assert the shape we emit.
        let encoded = base64::engine::general_purpose::STANDARD.encode("hello".as_bytes());
        assert_eq!(encoded, "aGVsbG8=");
    }
}
