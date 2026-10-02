//! Unified logging primitives — scrubbing writer, redaction newtype, and
//! a single subscriber init.
//!
//! ## What this guarantees
//!
//! - **Defense in depth**. Two layers stand between plugin code and the
//!   on-disk log:
//!   1. **Source**: wrap secret values in [`Redacted`] so `Debug`/`Display`
//!      never reveal them (`tracing::info!(token = ?Redacted::new(...))`).
//!      Memory is zeroed on drop.
//!   2. **Sink**: every serialised log line passes through [`scrub`],
//!      which rewrites well-known sensitive patterns (PVE API tokens,
//!      `Authorization: Bearer …`, `X-Api-Key: …`, JSON fields named
//!      `token`/`password`/`secret`/`api_key`/`token_secret`) to `***`
//!      *before* it reaches stderr or the on-disk log.
//!
//! - **Single setup**. Binaries call [`init`] once. EnvFilter + JSON +
//!   scrubbing writer + tee-to-file are all wired here so the recipe
//!   stays consistent across `orca`, future per-host daemons, and tests.
//!
//! ## What this does not catch
//!
//! Field names not on the keyword list (`apikey` vs `api_key`, custom
//! `x-foo-secret` headers, plaintext credentials in URL path segments).
//! Add patterns as needed; tests below pin the current coverage.

use anyhow::{Context, Result};
use regex::Regex;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

// ── Redaction newtype ──────────────────────────────────────────────────────

/// Wraps a secret-bearing value so `Debug`/`Display` never reveal it.
/// Memory is zeroed on drop.
///
/// ```ignore
/// tracing::info!(token = ?Redacted::new(api_key), "calling upstream");
/// // emits: token=Redacted(***)
/// ```
pub struct Redacted<T: Zeroize>(T);

impl<T: Zeroize> Redacted<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Plaintext access. Use sparingly and only at the wire layer.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Redacted(***)")
    }
}

impl<T: Zeroize> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T: Zeroize> Drop for Redacted<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// In-crate `Zeroize` to avoid pulling the upstream crate just for
/// `String`. Overwrites the buffer with zero bytes before drop.
pub trait Zeroize {
    fn zeroize(&mut self);
}

impl Zeroize for String {
    fn zeroize(&mut self) {
        // Writing zero bytes over an owned String's allocation is valid
        // UTF-8 (NULs are legal) and reaches the same bytes the secret
        // was stored in.
        let bytes = unsafe { self.as_bytes_mut() };
        for b in bytes.iter_mut() {
            *b = 0;
        }
        self.clear();
    }
}

// ── Sink-side scrub ────────────────────────────────────────────────────────

static SCRUB_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    // Order matters: longer / more-specific patterns first so they win
    // over the generic JSON-keyword catch.
    vec![
        // PVEAPIToken=user@realm!tokenid=uuid — both sides of the `=`
        // are sensitive (the token id is half of the credential).
        Regex::new(r#"(PVEAPIToken=)[^\s"']+"#).unwrap(),
        // Authorization headers: Bearer / Basic / Token / PVE / etc.
        Regex::new(r#"(?i)(authorization\s*[:=]\s*"?)([A-Za-z]+\s+)?[A-Za-z0-9._\-+/=]+"#).unwrap(),
        // X-Api-Key / X-Auth-Token header lines and JSON pairs.
        Regex::new(r#"(?i)(x-(?:api|auth)-(?:key|token)\s*[:=]\s*"?)[A-Za-z0-9._\-]+"#).unwrap(),
        // Generic JSON: "token": "...", "password": "...", "secret": "...",
        // "api_key": "...", "token_secret": "...". Quoted values only.
        Regex::new(
            r#"("(?:token|password|secret|api_key|apikey|token_secret|access_token|refresh_token)"\s*:\s*)"[^"]*""#,
        )
        .unwrap(),
    ]
});

/// Scrub a single line, rewriting any matched secret to `***`. Returns
/// `Cow::Borrowed` when nothing matched so the hot path stays
/// allocation-free.
pub fn scrub(line: &str) -> Cow<'_, str> {
    let mut out: Cow<'_, str> = Cow::Borrowed(line);
    for pat in SCRUB_PATTERNS.iter() {
        let replaced = match &out {
            Cow::Borrowed(s) => pat.replace_all(s, scrub_replacement),
            Cow::Owned(s) => Cow::Owned(pat.replace_all(s, scrub_replacement).into_owned()),
        };
        if let Cow::Owned(s) = replaced {
            out = Cow::Owned(s);
        }
    }
    out
}

fn scrub_replacement(caps: &regex::Captures<'_>) -> String {
    // Group 1 (if present) is a "keep" prefix — header name or JSON
    // `"token":` — that the regex matched but should remain verbatim.
    // The rest of the match is the secret, replaced by `***` (quoted
    // for JSON-shape preservation when group 1 ends with `:`).
    match caps.get(1) {
        Some(prefix) => {
            let prefix = prefix.as_str();
            if prefix.trim_end().ends_with(':') {
                format!("{prefix}\"***\"")
            } else {
                format!("{prefix}***")
            }
        }
        None => "***".to_string(),
    }
}

/// `std::io::Write` wrapper that scrubs each full line before forwarding
/// it to `inner`. Buffers partial lines so a `tracing-subscriber` event
/// split across multiple `write` calls still gets scrubbed atomically.
pub struct ScrubWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> ScrubWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(1024),
        }
    }
}

impl<W: Write> Write for ScrubWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let s = String::from_utf8_lossy(&line);
            let cleaned = scrub(&s);
            self.inner.write_all(cleaned.as_bytes())?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let s = String::from_utf8_lossy(&self.buf);
            let cleaned = scrub(&s);
            self.inner.write_all(cleaned.as_bytes())?;
            self.buf.clear();
        }
        self.inner.flush()
    }
}

impl<W: Write> Drop for ScrubWriter<W> {
    fn drop(&mut self) {
        _ = self.flush();
    }
}

// ── Log dedupe / throttle gates ──────────────────────────────────────────────
//
// Per-tick reconcile / topology / capability paths re-emit the *same* WARN
// every ~2s while a condition holds (missing runtime socket, cross-namespace
// access, RSS over ceiling). On a host where the condition is persistent that
// buries all signal — a live rc.4 host filled 145 MB of dev log at ~99% noise.
// These gates let a call site decide whether to actually emit, keyed by a
// caller-chosen string, without each site reinventing its own atomics.

/// One throttle record: when the key last fired and the interval it fired
/// under. The interval is stored per entry so the self-pruning sweep can decide
/// expiry correctly even when different call sites use different intervals.
#[derive(Clone, Copy)]
struct ThrottleEntry {
    last: Instant,
    interval: Duration,
}

static THROTTLE_STATE: LazyLock<Mutex<HashMap<String, ThrottleEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ONCE_STATE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Returns `true` at most once per `interval` for a given `key`, and always
/// the first time the key is seen. Intended to gate a repeating `warn!` on a
/// persistent condition: `if should_warn_throttled(key, dur) { warn!(...) }`.
/// Keys are caller-namespaced (e.g. `"reconcile:docker:list"`).
///
/// SELF-BOUNDING: every call first drops entries whose interval has elapsed.
/// An expired entry would return `true` and be re-inserted on its next hit
/// anyway, so retaining it is pointless — and retaining it is exactly how this
/// map leaked. A caller that (mis)keys by a *varying* string — e.g. one that
/// embeds a changing error message — used to mint a new permanent entry on
/// every ~2s tick, growing the map without bound for the life of the daemon
/// (the fleet RSS leak). Pruning caps the map at the keys seen within one
/// interval window regardless of key cardinality, so no call site can leak it.
pub fn should_warn_throttled(key: &str, interval: Duration) -> bool {
    let now = Instant::now();
    let mut map = THROTTLE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, e| now.duration_since(e.last) < e.interval);
    match map.get(key) {
        Some(e) if now.duration_since(e.last) < interval => false,
        _ => {
            map.insert(
                key.to_string(),
                ThrottleEntry {
                    last: now,
                    interval,
                },
            );
            true
        }
    }
}

/// Current number of live throttle keys. Exposed for the leak-watch monitor /
/// diagnostics so a runaway key cardinality is observable at runtime. With the
/// self-pruning sweep this stays bounded; a large value flags a caller keying
/// by a high-cardinality (varying) string.
pub fn throttle_key_count() -> usize {
    THROTTLE_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

/// Returns `true` only the first time `key` is seen for the life of the
/// process, `false` on every later call. Intended to surface a distinct event
/// exactly once: `if should_warn_once(key) { warn!(...) }`.
pub fn should_warn_once(key: &str) -> bool {
    let mut set = ONCE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    set.insert(key.to_string())
}

// ── Unified init ───────────────────────────────────────────────────────────

/// Logging setup options for binary entry points.
pub struct LogInit<'a> {
    /// Env var name that overrides the default filter (e.g. `"ORCA_LOG"`).
    pub env_var: &'a str,
    /// Default filter applied when the env var is unset / invalid.
    pub default_filter: &'a str,
    /// Optional path for a tee'd append-mode log file. `None` =
    /// stderr-only.
    pub tee_path: Option<&'a str>,
    /// Rotate the tee once it passes this many bytes. `None` = the default
    /// [`DEFAULT_TEE_MAX_BYTES`].
    ///
    /// Unrotated, this file grew to 36 MB on mint and its launchd-captured
    /// twin to 123 MB, neither ever pruned (#563). A daemon that fills a disk
    /// with its own logs is an outage it caused itself.
    pub tee_max_bytes: Option<u64>,
    /// How many rotated generations to keep (`daemon.jsonl.1` ..). `None` =
    /// the default [`DEFAULT_TEE_KEEP`].
    pub tee_keep: Option<u8>,
}

/// Rotate the tee at 32 MiB. Large enough to hold a long incident in one
/// file, small enough that `keep` generations stay bounded well under a GB.
pub const DEFAULT_TEE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Keep 5 rotated generations — matching the fleet's "keep last N" habit and
/// capping the tee's worst case at roughly 6 x 32 MiB.
pub const DEFAULT_TEE_KEEP: u8 = 5;

/// The rename sequence a rotation performs, oldest first, as
/// `(from, to)` pairs. `<path>.<keep>` is dropped rather than renamed.
///
/// Pure so the ordering is testable: performed newest-first would clobber,
/// so the pairs MUST be applied in the order returned (oldest first).
pub fn rotation_plan(
    path: &std::path::Path,
    keep: u8,
) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
    let mut plan = Vec::new();
    if keep == 0 {
        return plan;
    }
    let nth = |n: u8| -> std::path::PathBuf {
        let mut s = path.as_os_str().to_os_string();
        s.push(format!(".{n}"));
        std::path::PathBuf::from(s)
    };
    // .4 -> .5, .3 -> .4, ... .1 -> .2, then the live file -> .1
    for n in (1..keep).rev() {
        plan.push((nth(n), nth(n + 1)));
    }
    plan.push((path.to_path_buf(), nth(1)));
    plan
}

/// Oldest generation, removed before the shuffle so `keep` is a hard cap.
pub fn oldest_generation(path: &std::path::Path, keep: u8) -> Option<std::path::PathBuf> {
    if keep == 0 {
        return None;
    }
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{keep}"));
    Some(std::path::PathBuf::from(s))
}

/// Perform the rotation: drop the oldest, shuffle the rest up, and return a
/// freshly created live file. Best-effort on each rename — a rotation that
/// cannot complete must not take logging down with it.
fn rotate(path: &std::path::Path, keep: u8) -> io::Result<std::fs::File> {
    if let Some(oldest) = oldest_generation(path, keep) {
        _ = std::fs::remove_file(&oldest);
    }
    for (from, to) in rotation_plan(path, keep) {
        _ = std::fs::rename(&from, &to);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Install the global tracing subscriber: JSON-line output, EnvFilter,
/// scrubbing writer wrapping `stderr` (+ tee file when set).
///
/// Idempotent across repeated calls — second call is a no-op.
pub fn init(opts: LogInit<'_>) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env(opts.env_var)
        .unwrap_or_else(|_| EnvFilter::new(opts.default_filter));

    // Open the tee file ONCE, not per log event. `make_writer` is a
    // `MakeWriter` that tracing invokes on every event; opening the file in
    // there ran a create/open syscall (and leaked the churn the fleet leak
    // watch flagged) for every log line. Open here and share an append-mode
    // handle — O_APPEND keeps concurrent writes atomically positioned, and the
    // Mutex serialises the interleave.
    let tee_max = opts.tee_max_bytes.unwrap_or(DEFAULT_TEE_MAX_BYTES);
    let tee_keep = opts.tee_keep.unwrap_or(DEFAULT_TEE_KEEP);
    let tee_file: Option<Arc<Mutex<RotatingFile>>> = opts.tee_path.and_then(|path| {
        RotatingFile::open(std::path::PathBuf::from(path), tee_max, tee_keep)
            .ok()
            .map(|f| Arc::new(Mutex::new(f)))
    });
    let make_writer = move || -> ScrubWriter<Box<dyn Write + Send>> {
        let stderr: Box<dyn Write + Send> = Box::new(io::stderr());
        let writer: Box<dyn Write + Send> = match &tee_file {
            Some(file) => Box::new(Tee(stderr, SharedFile(file.clone()))),
            None => stderr,
        };
        ScrubWriter::new(writer)
    };

    let result = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_target(true)
        .with_writer(make_writer)
        .try_init();

    // try_init returns Err if a subscriber is already set — that's the
    // idempotent path, not a real failure.
    _ = result;
    Ok(())
}

/// An append-mode log file that rotates once it passes `max_bytes`.
///
/// Size is tracked in-process and seeded from the file's length at open, so
/// the steady state costs no syscall per line — only the write itself. A
/// `stat` per event is what made the previous open-per-line implementation
/// expensive, and this must not reintroduce it.
struct RotatingFile {
    path: std::path::PathBuf,
    file: std::fs::File,
    written: u64,
    max_bytes: u64,
    keep: u8,
}

impl RotatingFile {
    fn open(path: std::path::PathBuf, max_bytes: u64, keep: u8) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // Seed from the existing length: a daemon restarting onto an already
        // oversized file must rotate on its first write, not append forever.
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RotatingFile {
            path,
            file,
            written,
            max_bytes,
            keep,
        })
    }

    /// True once the live file has passed the cap. `max_bytes == 0` disables
    /// rotation entirely, for callers that manage the file themselves.
    fn should_rotate(&self) -> bool {
        self.max_bytes > 0 && self.written >= self.max_bytes
    }
}

impl Write for RotatingFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.should_rotate()
            && let Ok(fresh) = rotate(&self.path, self.keep)
        {
            self.file = fresh;
            self.written = 0;
        }
        let n = self.file.write(b)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// A shared append-mode file handle for the tee side. Cloned per
/// `make_writer` call but backed by a single open file — the write lock
/// serialises interleaved log lines from concurrent tasks.
struct SharedFile(Arc<Mutex<RotatingFile>>);

impl Write for SharedFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        // A poisoned lock still holds a valid file; recover the guard so a
        // panicking logger elsewhere doesn't silence the tee.
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f.flush()
    }
}

struct Tee<A: Write, B: Write>(A, B);

impl<A: Write, B: Write> Write for Tee<A, B> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        // Best-effort on the tee side: a failed file write doesn't
        // sink the primary stderr write.
        _ = self.1.write_all(b);
        self.0.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        _ = self.1.flush();
        self.0.flush()
    }
}

/// Tracing subscriber init helper used by binaries that need finer
/// control than [`init`] (e.g. custom layer stacks). Returns the
/// `EnvFilter` so callers can compose their own layer set.
pub fn env_filter(env_var: &str, default_filter: &str) -> Result<tracing_subscriber::EnvFilter> {
    Ok(
        tracing_subscriber::EnvFilter::try_from_env(env_var).unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::try_new(default_filter)
                .context("invalid default log filter")
                .unwrap()
        }),
    )
}

#[cfg(test)]
mod tests {
    // ── #563: the daemon must not fill the disk with its own logs ───────

    #[test]
    fn the_plan_shuffles_oldest_first_so_nothing_is_clobbered() {
        let p = std::path::Path::new("/l/daemon.jsonl");
        let plan = super::rotation_plan(p, 3);
        let as_str: Vec<(String, String)> = plan
            .iter()
            .map(|(a, b)| (a.display().to_string(), b.display().to_string()))
            .collect();
        // Applied in THIS order: .2->.3 before .1->.2 before live->.1.
        // Reversed, each rename would overwrite the generation not yet moved.
        assert_eq!(
            as_str,
            vec![
                ("/l/daemon.jsonl.2".into(), "/l/daemon.jsonl.3".into()),
                ("/l/daemon.jsonl.1".into(), "/l/daemon.jsonl.2".into()),
                ("/l/daemon.jsonl".into(), "/l/daemon.jsonl.1".into()),
            ]
        );
    }

    #[test]
    fn keep_zero_rotates_nothing() {
        let p = std::path::Path::new("/l/d.jsonl");
        assert!(super::rotation_plan(p, 0).is_empty());
        assert_eq!(super::oldest_generation(p, 0), None);
    }

    #[test]
    fn the_oldest_generation_is_the_one_dropped() {
        let p = std::path::Path::new("/l/d.jsonl");
        assert_eq!(
            super::oldest_generation(p, 5)
                .unwrap()
                .display()
                .to_string(),
            "/l/d.jsonl.5"
        );
    }

    #[test]
    fn nothing_is_lost_across_a_rotation_boundary() {
        // The contract is NOT "no line is ever lost" — `keep` exists precisely
        // to discard old data (see the next test). What must hold is that a
        // rotation itself loses nothing: everything written stays readable
        // across the boundary while it is still within the retained window.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.jsonl");
        // Cap sized so 20 lines span ~2 rotations, well inside keep=5.
        let mut f = super::RotatingFile::open(path.clone(), 400, 5).unwrap();
        for i in 0..20 {
            writeln!(f, "line {i:03} padded out to make this line long enough").unwrap();
        }
        f.flush().unwrap();
        assert!(
            path.exists(),
            "a live file must always exist after rotation"
        );
        let rotated: Vec<_> = (1..=5)
            .map(|n| dir.path().join(format!("daemon.jsonl.{n}")))
            .filter(|p| p.exists())
            .collect();
        assert!(!rotated.is_empty(), "nothing rotated; the cap did not fire");

        let mut all = std::fs::read_to_string(&path).unwrap();
        for r in &rotated {
            all.push_str(&std::fs::read_to_string(r).unwrap());
        }
        for i in 0..20 {
            assert!(all.contains(&format!("line {i:03}")), "lost line {i}");
        }
    }

    #[test]
    fn keep_discards_the_oldest_data_on_purpose() {
        // The complement of the test above, and the reason the cap works at
        // all: once `keep` generations are full, the oldest lines GO. A
        // rotation scheme that kept everything would not bound anything.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut f = super::RotatingFile::open(path.clone(), 50, 2).unwrap();
        writeln!(f, "FIRST-LINE-MARKER padded out so it fills a generation").unwrap();
        for i in 0..40 {
            writeln!(f, "subsequent line {i} also padded out to force rotation").unwrap();
        }
        f.flush().unwrap();
        let mut all = std::fs::read_to_string(&path).unwrap();
        for n in 1..=2 {
            let g = dir.path().join(format!("d.jsonl.{n}"));
            if g.exists() {
                all.push_str(&std::fs::read_to_string(g).unwrap());
            }
        }
        assert!(
            !all.contains("FIRST-LINE-MARKER"),
            "the oldest line must age out, or the cap bounds nothing"
        );
    }

    #[test]
    fn generations_are_capped_at_keep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut f = super::RotatingFile::open(path.clone(), 32, 2).unwrap();
        for i in 0..60 {
            writeln!(f, "a fairly long line number {i} to force many rotations").unwrap();
        }
        f.flush().unwrap();
        // .3 must never exist with keep=2 — otherwise "keep" is advisory and
        // the disk still fills, just more slowly.
        assert!(!dir.path().join("d.jsonl.3").exists());
        assert!(!dir.path().join("d.jsonl.4").exists());
    }

    #[test]
    fn an_already_oversized_file_rotates_on_the_first_write() {
        // A daemon restarting onto the 36 MB file we actually have must not
        // append to it forever; size is seeded from the file at open.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        std::fs::write(&path, "x".repeat(5000)).unwrap();
        let mut f = super::RotatingFile::open(path.clone(), 1000, 2).unwrap();
        assert!(f.should_rotate(), "seeded size must trip the cap");
        writeln!(f, "fresh").unwrap();
        f.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "fresh");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("d.jsonl.1"))
                .unwrap()
                .len(),
            5000,
            "the old content must survive as generation 1"
        );
    }

    #[test]
    fn a_zero_cap_disables_rotation() {
        // Escape hatch for a caller managing the file itself (logrotate, etc).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut f = super::RotatingFile::open(path.clone(), 0, 3).unwrap();
        for _ in 0..200 {
            writeln!(f, "never rotated").unwrap();
        }
        f.flush().unwrap();
        assert!(!dir.path().join("d.jsonl.1").exists());
    }

    use super::*;

    #[test]
    fn redacted_debug_hides_value() {
        let r = Redacted::new(String::from("topsecret"));
        assert_eq!(format!("{r:?}"), "Redacted(***)");
        assert_eq!(format!("{r}"), "***");
        assert_eq!(r.expose(), "topsecret");
    }

    #[test]
    fn zeroize_string_clears_buffer() {
        let mut s = String::from("secret");
        s.zeroize();
        assert!(s.is_empty());
    }

    #[test]
    fn scrub_pve_api_token_full_credential() {
        let line = r#"calling PVEAPIToken=user@pve!auto=deadbeef-1111-2222-3333-444444444444"#;
        let out = scrub(line);
        assert!(out.contains("PVEAPIToken=***"));
        assert!(!out.contains("deadbeef"));
        assert!(!out.contains("auto"));
    }

    #[test]
    fn scrub_bearer_authorization_header() {
        let jwt_lookalike = ["aaa", "bbb", "ccc"].join(".");
        let line = format!("header: Authorization: Bearer {jwt_lookalike}");
        let out = scrub(&line);
        assert!(out.contains("Authorization:"));
        assert!(out.contains("***"));
        assert!(!out.contains(&jwt_lookalike));
    }

    #[test]
    fn scrub_x_api_key_header() {
        let line = r#"sending X-Api-Key: abc-123-xyz to upstream"#;
        let out = scrub(line);
        assert!(out.contains("X-Api-Key:"));
        assert!(out.contains("***"));
        assert!(!out.contains("abc-123-xyz"));
    }

    #[test]
    fn scrub_json_token_field() {
        let line = r#"{"event":"login","token":"abc.def.ghi","ok":true}"#;
        let out = scrub(line);
        assert!(out.contains(r#""token":"***""#));
        assert!(!out.contains("abc.def.ghi"));
        assert!(out.contains(r#""event":"login""#));
        assert!(out.contains(r#""ok":true"#));
    }

    #[test]
    fn scrub_json_password_secret_api_key_fields() {
        let pw = "hunter2";
        let sec = ["s", "k", "_live_xyz"].concat();
        let ak = "AIzaSy";
        let line = format!(r#"{{"password":"{pw}","secret":"{sec}","api_key":"{ak}"}}"#);
        let out = scrub(&line);
        assert!(!out.contains(pw));
        assert!(!out.contains(&sec));
        assert!(!out.contains(ak));
        // 3 distinct field replacements
        assert_eq!(out.matches("\"***\"").count(), 3);
    }

    #[test]
    fn scrub_clean_line_returns_borrowed() {
        let line = r#"{"event":"tick","count":42}"#;
        let out = scrub(line);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn scrub_writer_passes_through_clean_lines() {
        let mut sink = Vec::new();
        {
            let mut w = ScrubWriter::new(&mut sink);
            writeln!(w, r#"{{"event":"ok","count":1}}"#).unwrap();
            w.flush().unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert!(out.contains(r#""count":1"#));
    }

    #[test]
    fn scrub_writer_redacts_secret_in_full_line() {
        let mut sink = Vec::new();
        {
            let mut w = ScrubWriter::new(&mut sink);
            writeln!(w, r#"{{"event":"login","password":"hunter2","ok":true}}"#).unwrap();
            w.flush().unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert!(!out.contains("hunter2"));
        assert!(out.contains(r#""password":"***""#));
    }

    #[test]
    fn should_warn_throttled_suppresses_within_interval_and_allows_after() {
        let key = "test:throttle:within";
        // First call for a key always fires.
        assert!(should_warn_throttled(key, Duration::from_secs(300)));
        // Immediate repeat within the interval is suppressed.
        assert!(!should_warn_throttled(key, Duration::from_secs(300)));
        // A zero interval always re-allows (the elapsed time is never < 0).
        assert!(should_warn_throttled(key, Duration::from_secs(0)));
    }

    #[test]
    fn should_warn_throttled_is_self_bounding_under_varying_keys() {
        // Regression for the fleet RSS leak: a caller that keys by a VARYING
        // string (e.g. one embedding a changing error message) must NOT grow the
        // throttle map without bound. Each of these unique keys is inserted with
        // a zero interval, so it is already expired by the next call and the
        // self-pruning sweep drops it. After N inserts the live key count stays
        // tiny instead of retaining all N forever.
        for i in 0..2000 {
            let _ = should_warn_throttled(&format!("leak:regression:{i}"), Duration::from_secs(0));
        }
        // One more call sweeps the last expired zero-interval entry.
        let _ = should_warn_throttled("leak:regression:final", Duration::from_secs(300));
        let live = throttle_key_count();
        assert!(
            live < 50,
            "throttle map must stay bounded under varying keys; got {live} live keys"
        );
    }

    #[test]
    fn should_warn_once_fires_only_first_time() {
        let key = "test:once:first";
        assert!(should_warn_once(key));
        assert!(!should_warn_once(key));
        assert!(!should_warn_once(key));
        // A distinct key is independent.
        assert!(should_warn_once("test:once:second"));
    }

    #[test]
    fn scrub_writer_buffers_partial_line_until_newline() {
        let mut sink = Vec::new();
        {
            let mut w = ScrubWriter::new(&mut sink);
            w.write_all(br#"{"event":"x","token":"abc"#).unwrap();
            // No newline yet — buffer holds the partial line; nothing
            // hits the inner writer until we close the line. Drop+inspect
            // happens after the block to satisfy the borrow checker.
            w.write_all(b"\"}\n").unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert!(out.contains(r#""token":"***""#));
        assert!(!out.contains(r#""token":"abc""#));
    }
}
