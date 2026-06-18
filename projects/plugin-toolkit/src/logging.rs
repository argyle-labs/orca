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
use std::fmt;
use std::io::{self, Write};
use std::sync::LazyLock;

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
}

/// Install the global tracing subscriber: JSON-line output, EnvFilter,
/// scrubbing writer wrapping `stderr` (+ tee file when set).
///
/// Idempotent across repeated calls — second call is a no-op.
pub fn init(opts: LogInit<'_>) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env(opts.env_var)
        .unwrap_or_else(|_| EnvFilter::new(opts.default_filter));

    let tee_path = opts.tee_path.map(str::to_string);
    let make_writer = move || -> ScrubWriter<Box<dyn Write + Send>> {
        let stderr: Box<dyn Write + Send> = Box::new(io::stderr());
        let writer: Box<dyn Write + Send> = match tee_path.as_deref() {
            Some(path) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => Box::new(Tee(stderr, file)),
                Err(_) => stderr,
            },
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
