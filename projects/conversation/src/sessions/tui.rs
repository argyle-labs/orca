use ::model::OutputSink;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

// ─── TUI App State ──────────────────────────────────────────────────────────

/// Cap on retained scrollback lines. Older lines are dropped from the front
/// so a long streaming session can't grow the buffer without bound. Chosen to
/// comfortably exceed a few full-terminal screens of context.
const MAX_LINES: usize = 10_000;

/// Cap on retained input history entries.
const MAX_HISTORY: usize = 1_000;

pub struct TuiApp {
    /// Completed output lines.
    lines: Vec<String>,
    /// Partial line being built by streaming tokens.
    partial: String,
    /// User input buffer.
    input: String,
    /// Cursor position in input (byte offset — ASCII-safe for typical commands).
    cursor: usize,
    /// Scroll offset (lines from bottom; 0 = pinned to bottom).
    scroll: u16,
    /// Auto-scroll to bottom on new output.
    auto_scroll: bool,
    /// Command history.
    history: Vec<String>,
    /// History browsing index (None = not browsing).
    history_idx: Option<usize>,
    /// Saved input when browsing history.
    saved_input: String,
    /// Whether a chat is in progress.
    pub busy: bool,
    /// Prompt prefix (e.g. "☯ orca ›").
    prompt: String,
    /// Whether the app should quit.
    pub should_quit: bool,
}

impl TuiApp {
    pub fn new(prompt: &str) -> Self {
        TuiApp {
            lines: Vec::new(),
            partial: String::new(),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            auto_scroll: true,
            history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            busy: false,
            prompt: prompt.to_string(),
            should_quit: false,
        }
    }

    /// Append raw text from the output channel. Handles embedded newlines.
    pub fn append(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.partial);
                self.lines.push(line);
            } else {
                self.partial.push(ch);
            }
        }
        self.trim_scrollback();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Push a complete line.
    pub fn push_line(&mut self, line: impl Into<String>) {
        if !self.partial.is_empty() {
            self.lines.push(std::mem::take(&mut self.partial));
        }
        self.lines.push(line.into());
        self.trim_scrollback();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Drop oldest lines once `MAX_LINES` is exceeded so long sessions don't
    /// grow the scrollback Vec without bound.
    fn trim_scrollback(&mut self) {
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
        }
    }

    fn scroll_to_bottom(&mut self) {
        // scroll is computed dynamically during render, just set the flag
        self.auto_scroll = true;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> TuiAction {
        match (key.modifiers, key.code) {
            // Quit
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self.busy {
                    return TuiAction::Cancel;
                }
                if self.input.is_empty() {
                    self.should_quit = true;
                    return TuiAction::Quit;
                }
                self.input.clear();
                self.cursor = 0;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) if self.input.is_empty() => {
                self.should_quit = true;
                return TuiAction::Quit;
            }

            // Line editing
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.input.drain(..self.cursor);
                self.cursor = 0;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.input.truncate(self.cursor);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('a')) | (_, KeyCode::Home) => {
                self.cursor = 0;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('e')) | (_, KeyCode::End) => {
                self.cursor = self.input.len();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                // Delete word backward
                let before = &self.input[..self.cursor];
                let trimmed = before.trim_end();
                let new_end = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                self.input.drain(new_end..self.cursor);
                self.cursor = new_end;
            }

            // Submit
            (_, KeyCode::Enter) => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return TuiAction::None;
                }
                self.history.push(text.clone());
                if self.history.len() > MAX_HISTORY {
                    let excess = self.history.len() - MAX_HISTORY;
                    self.history.drain(..excess);
                }
                self.history_idx = None;
                self.input.clear();
                self.cursor = 0;
                return TuiAction::Submit(text);
            }

            // Backspace / Delete — step back to char boundary
            (_, KeyCode::Backspace) if self.cursor > 0 => {
                let prev = self.input[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.cursor = prev;
                self.input.remove(self.cursor);
            }
            (_, KeyCode::Delete) if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
            }

            // Cursor movement — step by char boundaries, not raw bytes
            (_, KeyCode::Left) if self.cursor > 0 => {
                // Walk back to the start of the previous UTF-8 char
                let prev = self.input[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.cursor = prev;
            }
            (_, KeyCode::Right) if self.cursor < self.input.len() => {
                // Advance by the byte length of the current char
                let c = self.input[self.cursor..].chars().next().unwrap_or('\0');
                self.cursor += c.len_utf8();
            }

            // Scroll (Shift+Arrow — must come before wildcard Up/Down)
            (KeyModifiers::SHIFT, KeyCode::Up) => {
                self.scroll = self.scroll.saturating_add(1);
                self.auto_scroll = false;
            }
            (KeyModifiers::SHIFT, KeyCode::Down) => {
                self.scroll = self.scroll.saturating_sub(1);
                if self.scroll == 0 {
                    self.auto_scroll = true;
                }
            }

            // History
            (_, KeyCode::Up) => {
                if self.history.is_empty() {
                    return TuiAction::None;
                }
                match self.history_idx {
                    None => {
                        self.saved_input = self.input.clone();
                        let idx = self.history.len() - 1;
                        self.history_idx = Some(idx);
                        self.input = self.history[idx].clone();
                    }
                    Some(0) => {} // already at oldest
                    Some(idx) => {
                        let new_idx = idx - 1;
                        self.history_idx = Some(new_idx);
                        self.input = self.history[new_idx].clone();
                    }
                }
                self.cursor = self.input.len();
            }
            (_, KeyCode::Down) => {
                match self.history_idx {
                    None => {}
                    Some(idx) if idx + 1 >= self.history.len() => {
                        self.history_idx = None;
                        self.input = std::mem::take(&mut self.saved_input);
                    }
                    Some(idx) => {
                        let new_idx = idx + 1;
                        self.history_idx = Some(new_idx);
                        self.input = self.history[new_idx].clone();
                    }
                }
                self.cursor = self.input.len();
            }

            // Scroll
            (_, KeyCode::PageUp) => {
                self.scroll = self.scroll.saturating_add(10);
                self.auto_scroll = false;
            }
            (_, KeyCode::PageDown) => {
                self.scroll = self.scroll.saturating_sub(10);
                if self.scroll == 0 {
                    self.auto_scroll = true;
                }
            }
            // Character input
            (_, KeyCode::Char(c)) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }

            _ => {}
        }
        TuiAction::None
    }
}

pub enum TuiAction {
    Submit(String),
    Cancel,
    Quit,
    None,
}

// ─── Rendering ───────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, app: &TuiApp) {
    let total_width = f.area().width;
    let input_height = input_box_height(app, total_width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(input_height)])
        .split(f.area());

    render_output(f, app, chunks[0]);
    render_input(f, app, chunks[1]);
}

/// How many rows the input box needs: borders (2) + wrapped content rows.
/// Height is driven by the CURSOR position, not the text length, so the box
/// always has room for the cursor row — even when text exactly fills the
/// last content row (which would place cursor_row on the border otherwise).
fn input_box_height(app: &TuiApp, total_width: u16) -> u16 {
    let inner_width = total_width.saturating_sub(2) as usize; // subtract borders
    if inner_width == 0 {
        return 3;
    }
    let prefix_cols = app.prompt.width() + 1; // prompt + space
    let col_offset = prefix_cols + app.input[..app.cursor].width();
    let cursor_row = col_offset / inner_width;
    // cursor_row is 0-indexed; we need at least cursor_row+1 content rows
    let content_rows = (cursor_row + 1).max(1);
    (content_rows as u16) + 2 // +2 for borders
}

fn render_output(f: &mut Frame, app: &TuiApp, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize; // borders
    let inner_width = area.width.saturating_sub(2) as usize; // borders

    // Strip ANSI first so we can measure visual widths accurately.
    let stripped: Vec<String> = {
        let mut v: Vec<String> = app.lines.iter().map(|s| strip_ansi(s)).collect();
        if !app.partial.is_empty() {
            v.push(strip_ansi(&app.partial));
        }
        v
    };

    // Total rendered rows, accounting for line wrapping.
    let total_display_rows: usize = stripped
        .iter()
        .map(|s| {
            let w = s.width();
            if inner_width == 0 || w == 0 {
                1
            } else {
                w.div_ceil(inner_width)
            }
        })
        .sum();

    let display: Vec<Line> = stripped.iter().map(|s| Line::from(s.as_str())).collect();

    // Compute scroll position in display rows (not raw line count).
    let scroll_from_top = if app.auto_scroll {
        total_display_rows.saturating_sub(inner_height)
    } else {
        total_display_rows
            .saturating_sub(inner_height)
            .saturating_sub(app.scroll as usize)
    };

    let title = if app.busy {
        " output (running…) "
    } else {
        " output "
    };

    let paragraph = Paragraph::new(display)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(if app.busy {
                    Color::Yellow
                } else {
                    Color::DarkGray
                })),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll_from_top as u16, 0));

    f.render_widget(paragraph, area);
}

fn render_input(f: &mut Frame, app: &TuiApp, area: Rect) {
    let style = if app.busy {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };

    let input_widget = Paragraph::new(Line::from(vec![
        Span::styled(&app.prompt, Style::default().fg(Color::Cyan)),
        Span::raw(" "),
        Span::styled(&app.input, style),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" input ")
            .border_style(Style::default().fg(Color::Cyan)),
    )
    .wrap(Wrap { trim: false });

    f.render_widget(input_widget, area);

    // Cursor position accounting for line wrapping.
    // Column offset within the inner area = prompt_cols + 1 (space) + input_cols_at_cursor.
    let inner_width = area.width.saturating_sub(2) as usize;
    if inner_width == 0 {
        return;
    }
    let prefix_cols = app.prompt.width() + 1;
    let input_cols_at_cursor = app.input[..app.cursor].width();
    let col_offset = prefix_cols + input_cols_at_cursor;
    let cursor_row = (col_offset / inner_width) as u16;
    let cursor_col = (col_offset % inner_width) as u16;
    let cursor_x = area.x + 1 + cursor_col;
    let cursor_y = area.y + 1 + cursor_row;
    if cursor_x < area.x + area.width - 1 && cursor_y < area.y + area.height - 1 {
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

// ─── TUI OutputSink ──────────────────────────────────────────────────────────

/// Create an OutputSink that sends text through a channel to the TUI.
pub fn tui_sink(tx: mpsc::UnboundedSender<String>) -> OutputSink {
    let writer = TuiWriter(tx);
    Arc::new(Mutex::new(Box::new(writer)))
}

struct TuiWriter(mpsc::UnboundedSender<String>);

impl Write for TuiWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s = String::from_utf8_lossy(buf).to_string();
        self.0.send(s).ok();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ─── Terminal Setup ──────────────────────────────────────────────────────────

pub fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    _ = crossterm::terminal::disable_raw_mode();
    _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    );
    _ = terminal.show_cursor();
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Strip ANSI escape sequences. Good enough for SGR codes from `colored`.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            result.push(c);
        }
    }
    result
}
