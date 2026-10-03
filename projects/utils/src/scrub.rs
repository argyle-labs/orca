//! Secret scrubbing — the single source of truth for "what looks like a
//! secret" anywhere in the stack.
//!
//! This lives in `utils` (the dependency-free leaf) rather than in
//! `plugin_toolkit` so that *everything* can reach it: `contract::plan` redacts
//! dry-run inputs with it, the daemon's log sink scrubs lines with it, and
//! plugins get it through the toolkit's re-export. One keyword list, one place
//! — two divergent lists is exactly how one of them ends up missing a field.
//!
//! ## Two redactors, one list
//!
//! - [`redact_json`] / [`redact_json_in_place`] — **structured**, recursive
//!   over `serde_json::Value`. Dependency-free, always available. This is the
//!   right tool whenever the data is already parsed (plan inputs, tool args):
//!   it cannot be fooled by quoting or formatting the way regex-on-text can.
//! - [`scrub_line`] — **textual**, regex over an already-serialized log line.
//!   Gated behind the `scrub_text` feature so a thin consumer does not link
//!   `regex`. Catches things structure cannot: `Authorization:` headers,
//!   `PVEAPIToken=`, credentials pasted into a message body.
//!
//! Both decide sensitivity through [`is_sensitive_key`], so adding a keyword
//! in one place tightens every surface at once.
//!
//! ## Deliberate over-redaction
//!
//! The bias is toward redacting. Known false positives we accept: `notify`'s
//! dedupe `key`, and `storage.share`'s `credential` (a *reference* to a secret,
//! not the secret). Losing those from a plan is cosmetic; leaking an API key is
//! not. What we deliberately do NOT redact is key *names* that merely contain
//! "key" (`credKey`, `dataKey`, `admin_pubkey`) — redacting identifiers makes a
//! plan unreadable without protecting anything.

/// Marker substituted for a redacted **structured** value. Explicit and
/// greppable, so an operator reading a plan can tell "withheld" from "empty".
pub const REDACTED: &str = "<redacted>";

/// Marker substituted inside a redacted **log line**. Distinct from
/// [`REDACTED`] because log output is grepped by humans and tooling that
/// already expect `***`.
pub const REDACTED_TEXT: &str = "***";

/// Key names that are secret-bearing only as the WHOLE key.
///
/// `key` standing alone is an API key (`auth.session.create.key`), but as a
/// substring it is a lookup name (`credKey`, `dataKey`, `admin_pubkey`) — so
/// these must not be matched as fragments.
const SENSITIVE_EXACT: &[&str] = &[
    "key",
    "value",
    "credvalue",
    "pass",
    "cred",
    "auth",
    "pin",
    "otp",
    "cookie",
    "signature",
    "salt",
    "seed",
];

/// Fragments that make a key secret-bearing wherever they appear — `apiKey`,
/// `refresh_token`, `clientSecret`, `db_password`, `tokenSecret`.
const SENSITIVE_FRAGMENTS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "apikey",
    "token",
    "credential",
    "privatekey",
    "accesskey",
    "sessionkey",
    "signingkey",
    "authorization",
    "bearer",
];

/// Whether a field name should have its value withheld. Case- and
/// separator-insensitive: `api_key`, `apiKey`, `API-KEY` all match.
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    SENSITIVE_EXACT.contains(&normalized.as_str())
        || SENSITIVE_FRAGMENTS.iter().any(|f| normalized.contains(f))
}

/// Recursively replace every sensitive value with [`REDACTED`], walking nested
/// objects and arrays.
///
/// The key is **kept**. A plan's whole job is letting an operator confirm what
/// will be written, so a silently missing field makes the dry-run lie about its
/// own inputs.
#[allow(clippy::disallowed_types)]
pub fn redact_json_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_sensitive_key(k) {
                    *v = serde_json::Value::String(REDACTED.to_string());
                } else {
                    redact_json_in_place(v);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                redact_json_in_place(v);
            }
        }
        _ => {}
    }
}

/// Owned form of [`redact_json_in_place`].
#[allow(clippy::disallowed_types)]
pub fn redact_json(mut value: serde_json::Value) -> serde_json::Value {
    redact_json_in_place(&mut value);
    value
}

// ── Textual scrub (log lines) ───────────────────────────────────────────────

#[cfg(feature = "scrub_text")]
mod text {
    use super::{REDACTED_TEXT, SENSITIVE_EXACT, SENSITIVE_FRAGMENTS};
    use regex::Regex;
    use std::borrow::Cow;
    use std::sync::LazyLock;

    static SCRUB_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        // Order matters: longer / more-specific patterns first so they win
        // over the generic JSON-keyword catch.
        vec![
            // PVEAPIToken=user@realm!tokenid=uuid — both sides of the `=`
            // are sensitive (the token id is half of the credential).
            Regex::new(r#"(PVEAPIToken=)[^\s"']+"#).unwrap(),
            // Authorization headers: Bearer / Basic / Token / PVE / etc.
            Regex::new(r#"(?i)(authorization\s*[:=]\s*"?)([A-Za-z]+\s+)?[A-Za-z0-9._\-+/=]+"#)
                .unwrap(),
            // X-Api-Key / X-Auth-Token header lines and JSON pairs.
            Regex::new(r#"(?i)(x-(?:api|auth)-(?:key|token)\s*[:=]\s*"?)[A-Za-z0-9._\-]+"#)
                .unwrap(),
            // Generic quoted JSON pairs, built from the SHARED keyword list so
            // the text and structured scrubbers can never drift apart.
            Regex::new(&json_field_pattern()).unwrap(),
        ]
    });

    /// `("<sensitive key>"\s*:\s*)"..."` with the key alternation generated
    /// from the shared list — exact names anchored by the surrounding quotes,
    /// fragments allowed to sit inside a longer name (`apiKey`, `tokenSecret`).
    fn json_field_pattern() -> String {
        let mut alts: Vec<String> = SENSITIVE_EXACT.iter().map(|k| sep_tolerant(k)).collect();
        alts.extend(
            SENSITIVE_FRAGMENTS
                .iter()
                .map(|f| format!(r"[a-z0-9_.\-]*{}[a-z0-9_.\-]*", sep_tolerant(f))),
        );
        format!(r#"(?i)("(?:{})"\s*:\s*)"[^"]*""#, alts.join("|"))
    }

    /// Allow an optional separator between every character, so ONE list entry
    /// covers every spelling the wire uses: `apikey` matches `api_key`,
    /// `api-key` and `apiKey`. The structured redactor gets this for free by
    /// normalizing the key; the regex matches raw text, so without this
    /// `api_key` silently fell through the keyword catch.
    fn sep_tolerant(word: &str) -> String {
        let escaped: Vec<String> = word
            .chars()
            .map(|c| regex::escape(&c.to_string()))
            .collect();
        escaped.join(r"[_.\-]?")
    }

    /// Scrub a single line, rewriting any matched secret to `***`. Returns
    /// `Cow::Borrowed` when nothing matched so the hot path stays
    /// allocation-free.
    pub fn scrub_line(line: &str) -> Cow<'_, str> {
        let mut out: Cow<'_, str> = Cow::Borrowed(line);
        for pat in SCRUB_PATTERNS.iter() {
            let replaced = match &out {
                Cow::Borrowed(s) => pat.replace_all(s, replacement),
                Cow::Owned(s) => Cow::Owned(pat.replace_all(s, replacement).into_owned()),
            };
            if let Cow::Owned(s) = replaced {
                out = Cow::Owned(s);
            }
        }
        out
    }

    fn replacement(caps: &regex::Captures<'_>) -> String {
        // Group 1 (if present) is a "keep" prefix — header name or JSON
        // `"token":` — that the regex matched but should remain verbatim.
        // The rest of the match is the secret, replaced by `***` (quoted
        // for JSON-shape preservation when group 1 ends with `:`).
        match caps.get(1) {
            Some(prefix) => {
                let prefix = prefix.as_str();
                if prefix.trim_end().ends_with(':') {
                    format!("{prefix}\"{REDACTED_TEXT}\"")
                } else {
                    format!("{prefix}{REDACTED_TEXT}")
                }
            }
            None => REDACTED_TEXT.to_string(),
        }
    }
}

#[cfg(feature = "scrub_text")]
pub use text::scrub_line;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sensitive_keys_ignore_case_and_separators() {
        for k in [
            "value",
            "token",
            "api_key",
            "apiKey",
            "API-KEY",
            "password",
            "db_password",
            "clientSecret",
            "refresh_token",
            "passphrase",
            "private_key",
            "credential",
            "credValue",
            "key",
        ] {
            assert!(is_sensitive_key(k), "{k} must be treated as sensitive");
        }
    }

    #[test]
    fn identifier_names_are_not_redacted() {
        // Redacting a lookup NAME protects nothing and makes a plan unreadable.
        for k in [
            "credKey",
            "dataKey",
            "admin_pubkey",
            "keyboard",
            "name",
            "path",
            "backend",
            "dataValue",
            "monkey",
        ] {
            assert!(!is_sensitive_key(k), "{k} must stay visible");
        }
    }

    #[test]
    fn redacts_nested_objects_and_arrays_but_keeps_keys() {
        #[allow(clippy::disallowed_types)]
        let input = json!({
            "name": "canary.probe",
            "value": "CANARY-VALUE-12345",
            "nested": { "deep": { "apiKey": "CANARY-VALUE-12345" } },
            "list": [
                { "password": "CANARY-VALUE-12345" },
                [ { "refresh_token": "CANARY-VALUE-12345" } ]
            ]
        });
        let out = redact_json(input);
        let text = serde_json::to_string(&out).unwrap();
        assert!(
            !text.contains("CANARY-VALUE-12345"),
            "secret survived redaction: {text}"
        );
        assert_eq!(out["value"], REDACTED);
        assert_eq!(out["nested"]["deep"]["apiKey"], REDACTED);
        assert_eq!(out["list"][0]["password"], REDACTED);
        assert_eq!(out["list"][1][0]["refresh_token"], REDACTED);
        // Control: the non-sensitive field is untouched.
        assert_eq!(out["name"], "canary.probe");
    }

    #[test]
    fn non_sensitive_values_of_every_shape_are_untouched() {
        #[allow(clippy::disallowed_types)]
        let input = json!({
            "name": "ct/107",
            "count": 42,
            "enabled": true,
            "paths": ["/config", "/data"],
            "detail": null,
        });
        let out = redact_json(input.clone());
        assert_eq!(out, input, "over-redaction destroyed a usable plan");
    }

    /// Every separator spelling must be caught by BOTH redactors. The regex
    /// matches raw text, so a literal `apikey` fragment missed `api_key` —
    /// which is the exact spelling the old hand-written pattern listed
    /// explicitly.
    #[cfg(feature = "scrub_text")]
    #[test]
    fn text_scrub_catches_every_separator_spelling() {
        for key in [
            "api_key",
            "apikey",
            "api-key",
            "apiKey",
            "access_token",
            "refresh_token",
            "token_secret",
            "private_key",
            "client_secret",
        ] {
            let line = format!(r#"{{"{key}":"CANARY"}}"#);
            let out = scrub_line(&line);
            assert!(!out.contains("CANARY"), "{key} not scrubbed: {out}");
            assert!(
                is_sensitive_key(key),
                "{key} scrubbed in text but not structurally"
            );
        }
    }

    #[cfg(feature = "scrub_text")]
    #[test]
    fn text_scrub_shares_the_structured_key_list() {
        let line = r#"{"event":"x","clientSecret":"CANARY","dataValue":"keepme"}"#;
        let out = scrub_line(line);
        assert!(!out.contains("CANARY"), "{out}");
        assert!(out.contains("keepme"), "control field was redacted: {out}");
    }
}
