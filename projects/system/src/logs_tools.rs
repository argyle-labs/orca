//! Read-only tail/filter/paginate over this host's daemon log.
//!
//! `system.logs` reads `~/.orca/logs/daemon.log` from the END in bounded
//! chunks so a 47 MB log never lands in memory. The verb is
//! peer-dispatchable: `orca system logs --peer <host>` runs the same read on
//! a remote host over the mesh, making it the enabling primitive for
//! diagnosing a peer whose `system.detail` is failing.
//!
//! Paging goes backwards in time: `next_cursor` is the byte offset the
//! returned window started at; pass it back as `cursor` to fetch the
//! next-older window. `None` means the start of file was reached.

use contract::config::{APP_DAEMON_LOG_FILE, APP_LOGS_SUBDIR, APP_STATE_DIR};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};

const DEFAULT_TAIL: usize = 200;
const MIN_TAIL: usize = 1;
const MAX_TAIL: usize = 5_000;

const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const MIN_MAX_BYTES: u64 = 64 * 1024;
const MAX_MAX_BYTES: u64 = 64 * 1024 * 1024;

const CHUNK: usize = 64 * 1024;

/// Best-effort log severity, ordered least → most severe for the
/// minimum-level filter.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// Strip ANSI escape sequences (`ESC [ ... m` etc.) so a color-coded level
/// token still matches.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Consume until a letter (the final byte of a CSI/SGR sequence).
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Best-effort severity of a log line. Matches the first uppercase level
/// token the tracing fmt subscriber emits. `None` when no token is found
/// (continuation / stack-trace lines pass level filters).
fn parse_level(line: &str) -> Option<Level> {
    let clean = strip_ansi(line);
    for tok in clean.split(|c: char| !c.is_ascii_alphabetic()) {
        match tok {
            "ERROR" => return Some(Level::Error),
            "WARN" => return Some(Level::Warn),
            "INFO" => return Some(Level::Info),
            "DEBUG" => return Some(Level::Debug),
            "TRACE" => return Some(Level::Trace),
            _ => {}
        }
    }
    None
}

fn resolve_log_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    format!("{logs_dir}/{APP_DAEMON_LOG_FILE}")
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// Best-effort parsed severity (`error`|`warn`|`info`|`debug`|`trace`),
    /// or `None` for lines with no recognizable level token.
    pub level: Option<String>,
    /// The full raw line, trailing newline trimmed.
    pub message: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LogsArgs {
    /// Number of most-recent lines to return. Default 200, clamped to
    /// [1, 5000].
    #[arg(long)]
    pub tail: Option<usize>,
    /// Case-insensitive substring filter applied to each line.
    #[arg(long)]
    pub grep: Option<String>,
    /// Minimum level to keep: one of error|warn|info|debug|trace
    /// (case-insensitive). Lines with no parseable level always pass.
    #[arg(long)]
    pub level: Option<String>,
    /// Scan cap in bytes, measured back from the tail. Default 4 MiB,
    /// clamped to [64 KiB, 64 MiB].
    #[arg(long = "max-bytes")]
    pub max_bytes: Option<u64>,
    /// Opaque cursor: a byte offset (decimal) to read the window ENDING at
    /// that offset, for paging further back in time. Omit to read from EOF.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LogsOutput {
    /// The resolved log path that was read.
    pub path: String,
    /// The returned window, oldest first.
    pub lines: Vec<LogLine>,
    /// Byte offset to pass back as `cursor` for the next-older window.
    /// `None` when the start of file was reached.
    pub next_cursor: Option<String>,
    /// True if the `max_bytes` scan cap stopped the read before `tail` lines
    /// were gathered.
    pub truncated: bool,
}

/// Result of scanning the file backwards from `end`.
struct Scan {
    /// Raw lines (no trailing newline), oldest first.
    lines: Vec<String>,
    /// Byte offset the scan started reading at (window start).
    start: u64,
    /// True if the scan stopped on the byte cap rather than reaching enough
    /// lines or the start of file.
    truncated: bool,
    /// True if byte 0 was reached.
    hit_bof: bool,
}

/// Read backwards from `end` in [`CHUNK`] steps until `want` lines are held
/// or `max_bytes` are scanned, whichever comes first.
fn scan_backwards(
    file: &mut std::fs::File,
    end: u64,
    want: usize,
    max_bytes: u64,
) -> std::io::Result<Scan> {
    let mut pos = end;
    let mut buf: Vec<u8> = Vec::new();
    let mut scanned: u64 = 0;
    let mut truncated = false;

    loop {
        if pos == 0 {
            break;
        }
        if scanned >= max_bytes {
            truncated = true;
            break;
        }
        let step = CHUNK.min(pos as usize).min((max_bytes - scanned) as usize);
        let chunk_start = pos - step as u64;
        let mut chunk = vec![0u8; step];
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        pos = chunk_start;
        scanned += step as u64;

        // Count complete lines currently buffered (newlines seen). Once we
        // have more than `want` newlines the earliest full line is settled.
        let newlines = buf.iter().filter(|&&b| b == b'\n').count();
        if newlines > want {
            break;
        }
    }

    let hit_bof = pos == 0;
    let text = String::from_utf8_lossy(&buf);
    // Trim a single leading partial line unless we reached BOF (then the
    // first line is genuinely complete).
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if !hit_bof && !lines.is_empty() {
        lines.remove(0);
    }

    // `start` is the offset of the first line we are keeping.
    let dropped_prefix = if !hit_bof {
        // Bytes up to and including the first newline were dropped.
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => i as u64 + 1,
            None => buf.len() as u64,
        }
    } else {
        0
    };
    let start = pos + dropped_prefix;

    Ok(Scan {
        lines,
        start,
        truncated,
        hit_bof,
    })
}

/// Every provider this host has ever probed. `system.logs` reads the daemon
/// log tail for this host (or a peer via `--peer`), filtered and paginated.
///
/// Missing log file → empty `lines` with the resolved path and
/// `truncated=false` (a host may not have logged yet), never an error.
#[orca_tool(domain = "system", verb = "logs")]
async fn system_logs(args: LogsArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<LogsOutput> {
    let path = resolve_log_path();

    let tail = args.tail.unwrap_or(DEFAULT_TAIL).clamp(MIN_TAIL, MAX_TAIL);
    let max_bytes = args
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(MIN_MAX_BYTES, MAX_MAX_BYTES);
    let grep = args.grep.as_ref().map(|g| g.to_ascii_lowercase());
    let min_level = match args.level.as_deref() {
        Some(l) => match Level::parse(l) {
            Some(lvl) => Some(lvl),
            None => anyhow::bail!("invalid level: {l} (want error|warn|info|debug|trace)"),
        },
        None => None,
    };

    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LogsOutput {
                path,
                lines: Vec::new(),
                next_cursor: None,
                truncated: false,
            });
        }
        Err(e) => return Err(e.into()),
    };

    let file_len = file.metadata()?.len();
    let end = match args.cursor.as_deref() {
        Some(c) => c
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("invalid cursor: {c}"))?
            .min(file_len),
        None => file_len,
    };

    let scan = scan_backwards(&mut file, end, tail, max_bytes)?;

    // Apply filters, keep raw + parsed level.
    let filtered: Vec<LogLine> = scan
        .lines
        .into_iter()
        .filter(|line| {
            if let Some(g) = &grep
                && !line.to_ascii_lowercase().contains(g)
            {
                return false;
            }
            if let Some(min) = min_level
                && let Some(lvl) = parse_level(line)
                && lvl < min
            {
                return false;
            }
            true
        })
        .map(|line| LogLine {
            level: parse_level(&line).map(|l| l.as_str().to_string()),
            message: line,
        })
        .collect();

    // Keep only the last `tail` lines after filtering.
    let over = filtered.len().saturating_sub(tail);
    let lines: Vec<LogLine> = filtered.into_iter().skip(over).collect();

    // `truncated` = hit the byte cap before satisfying `tail`.
    let truncated = scan.truncated && lines.len() < tail;

    // A next (older) window exists whenever we did not reach BOF.
    let next_cursor = if scan.hit_bof {
        None
    } else {
        Some(scan.start.to_string())
    };

    Ok(LogsOutput {
        path,
        lines,
        next_cursor,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_log(lines: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        (dir, path)
    }

    fn read_lines(path: &std::path::Path, end: u64, want: usize, max_bytes: u64) -> Scan {
        let mut f = std::fs::File::open(path).unwrap();
        scan_backwards(&mut f, end, want, max_bytes).unwrap()
    }

    #[test]
    fn tail_returns_last_k_lines() {
        let all: Vec<String> = (0..100).map(|i| format!("line {i:03}")).collect();
        let refs: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
        let (_dir, path) = temp_log(&refs);
        let len = std::fs::metadata(&path).unwrap().len();

        let scan = read_lines(&path, len, 10, 1 << 20);
        let last10 = &scan.lines[scan.lines.len() - 10..];
        let got: Vec<&str> = last10.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            got,
            vec![
                "line 090", "line 091", "line 092", "line 093", "line 094", "line 095", "line 096",
                "line 097", "line 098", "line 099",
            ]
        );
        assert!(scan.hit_bof);
        assert!(!scan.truncated);
    }

    #[test]
    fn max_bytes_truncates_and_cursor_pages_older() {
        // 200 lines of a fixed width; a tiny cap forces truncation.
        let all: Vec<String> = (0..200).map(|i| format!("row-{i:04}-padpadpad")).collect();
        let refs: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
        let (_dir, path) = temp_log(&refs);
        let len = std::fs::metadata(&path).unwrap().len();

        // Cap of 64 KiB is bigger than the file; use the min cap and a big
        // tail so the cap is the binding limit. Each line ~20 bytes; 64 KiB
        // covers all 200, so force with a manual small cap below MIN via the
        // scanner directly.
        let cap = 256u64;
        let first = read_lines(&path, len, 5_000, cap);
        assert!(first.truncated);
        assert!(!first.hit_bof);
        assert!(first.start > 0);
        // Newest line must be present in the first (EOF) window.
        assert_eq!(first.lines.last().unwrap(), "row-0199-padpadpad");

        // Page older using the returned start as the next window end.
        let oldest_in_first = first.lines.first().unwrap().clone();
        let idx = all.iter().position(|l| *l == oldest_in_first).unwrap();
        let second = read_lines(&path, first.start, 5_000, cap);
        assert!(second.start < first.start);
        // The older window must end just before the first window began.
        assert_eq!(second.lines.last().unwrap(), &all[idx - 1]);
    }

    #[test]
    fn missing_line_level_parses_none() {
        assert!(parse_level("just some text with no level").is_none());
        assert_eq!(
            parse_level("2026-01-01 ERROR boom").map(|l| l.as_str()),
            Some("error")
        );
        assert_eq!(
            parse_level("2026-01-01 WARN hmm").map(|l| l.as_str()),
            Some("warn")
        );
    }

    #[test]
    fn strip_ansi_reveals_level() {
        let colored = "\u{1b}[32m INFO\u{1b}[0m starting up";
        assert_eq!(parse_level(colored).map(|l| l.as_str()), Some("info"));
    }

    #[test]
    fn level_ordering() {
        assert!(Level::Error > Level::Warn);
        assert!(Level::Warn > Level::Info);
        assert!(Level::Info > Level::Debug);
        assert!(Level::Debug > Level::Trace);
    }

    #[test]
    fn args_default_empty() {
        let a = LogsArgs::default();
        assert!(a.tail.is_none());
        assert!(a.grep.is_none());
        assert!(a.level.is_none());
        assert!(a.max_bytes.is_none());
        assert!(a.cursor.is_none());
    }

    #[test]
    fn args_deserialize_camel_case() {
        let a: LogsArgs =
            serde_json::from_str(r#"{"tail":50,"grep":"boom","level":"warn","maxBytes":131072}"#)
                .unwrap();
        assert_eq!(a.tail, Some(50));
        assert_eq!(a.grep.as_deref(), Some("boom"));
        assert_eq!(a.level.as_deref(), Some("warn"));
        assert_eq!(a.max_bytes, Some(131072));
    }
}
