//! Secret scrubbing — the single source of truth for "what looks like a
//! secret" anywhere in the stack.
//!
//! This lives in `utils` (the dependency-free leaf) rather than in
//! `plugin_toolkit` so that *everything* can reach it: `contract::plan` redacts
//! dry-run inputs with it, the daemon's log sink scrubs lines with it, tool-call
//! results are redacted with it, and plugins get it through the toolkit's
//! re-export.
//!
//! ## One vocabulary
//!
//! Every surface decides whether a name is secret through [`is_sensitive_key`],
//! backed by one table of whole-key, word and fragment entries, and substitutes
//! one marker, [`REDACTED`].
//!
//! ## Redactors
//!
//! - [`redact_json`] / [`redact_json_in_place`] — structured, recursive over
//!   `serde_json::Value`; every value under a sensitive key is replaced (plan
//!   inputs, tool args).
//! - [`redact_result`] / [`redact_result_in_place`] — structured, for tool-call
//!   results: only string values under sensitive keys are replaced, objects and
//!   arrays are walked, numbers and booleans survive, so the result keeps its
//!   shape. String values that are credential URIs or secret paths are replaced
//!   whole; every other string goes through [`redact_text`]. Needs `scrub_text`.
//! - [`scrub_line`] — the one text scrubber (needs `scrub_text`). Rewrites only
//!   the secret span of: JSON `"key": "value"` pairs, `NAME=value`,
//!   `--flag value` and `key: value` with a sensitive name, `Authorization` /
//!   `X-Api-Key` / `PVEAPIToken` headers, URI userinfo (`scheme://u:p@h` and
//!   bare `u:p@h`), webhook URLs, single-line PEM private keys, and well-known
//!   token shapes (GitHub, `sk-`, AWS `AKIA`/`ASIA`, Slack, JWT).
//! - [`redact_text`] — [`scrub_line`] over multi-line text, plus PEM private key
//!   blocks that span lines.
//!
//! ## Deliberate over-redaction
//!
//! The bias is toward redacting. Known false positives we accept: `notify`'s
//! dedupe `key`, and `storage.share`'s `credential` (a *reference* to a secret,
//! not the secret). Losing those from a plan is cosmetic; leaking an API key is
//! not. What we deliberately do NOT redact is key *names* that merely contain
//! "key" (`credKey`, `dataKey`, `admin_pubkey`, `hasKey`) — redacting identifiers
//! makes a plan unreadable without protecting anything.

#[cfg(feature = "scrub_text")]
use std::borrow::Cow;

pub const REDACTED: &str = "***";

#[derive(Clone, Copy)]
enum Scope {
    Whole,
    Word,
    Fragment,
}

const SENSITIVE: &[(&str, Scope)] = &[
    ("key", Scope::Whole),
    ("value", Scope::Whole),
    ("credvalue", Scope::Whole),
    ("pin", Scope::Whole),
    ("otp", Scope::Whole),
    ("cookie", Scope::Whole),
    ("signature", Scope::Whole),
    ("salt", Scope::Whole),
    ("seed", Scope::Whole),
    ("pass", Scope::Word),
    ("auth", Scope::Word),
    ("claim", Scope::Word),
    ("pw", Scope::Word),
    ("cred", Scope::Word),
    ("creds", Scope::Word),
    ("jwt", Scope::Word),
    ("hmac", Scope::Word),
    ("password", Scope::Fragment),
    ("passwd", Scope::Fragment),
    ("dbpass", Scope::Fragment),
    ("requirepass", Scope::Fragment),
    ("pwd", Scope::Fragment),
    ("passphrase", Scope::Fragment),
    ("secret", Scope::Fragment),
    ("apikey", Scope::Fragment),
    ("token", Scope::Fragment),
    ("credential", Scope::Fragment),
    ("privatekey", Scope::Fragment),
    ("accesskey", Scope::Fragment),
    ("sessionkey", Scope::Fragment),
    ("signingkey", Scope::Fragment),
    ("authorization", Scope::Fragment),
    ("basicauth", Scope::Fragment),
    ("bearer", Scope::Fragment),
];

const SECRET_URL_HOSTS: &[&str] = &[
    "hooks.slack.com",
    "discord.com",
    "discordapp.com",
    "ntfy.sh",
];

const SECRET_PATH_PREFIXES: &[&str] = &["/run/secrets/", "/var/run/secrets/"];

pub fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

const FLAG_PREFIXES: &[&str] = &[
    "no", "disable", "disabled", "without", "skip", "enable", "enabled", "allow", "use",
];

fn is_flag_name(key: &str) -> bool {
    let trimmed = key.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    trimmed
        .split_once(|c: char| !c.is_ascii_alphanumeric())
        .is_some_and(|(first, rest)| {
            FLAG_PREFIXES.iter().any(|p| first.eq_ignore_ascii_case(p)) && is_sensitive_key(rest)
        })
}

pub fn is_sensitive_key(key: &str) -> bool {
    if is_flag_name(key) {
        return false;
    }
    let normalized = normalize_key(key);
    SENSITIVE.iter().any(|(word, scope)| match scope {
        Scope::Whole => normalized == *word,
        Scope::Fragment => normalized.contains(word),
        Scope::Word => {
            normalized == *word
                || key
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|segment| segment.eq_ignore_ascii_case(word))
        }
    })
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

fn is_token_like(segment: &str) -> bool {
    segment.len() >= 16
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && segment.chars().any(|c| c.is_ascii_digit())
        && segment.chars().any(|c| c.is_ascii_alphabetic())
}

pub fn is_secret_value(value: &str) -> bool {
    if SECRET_PATH_PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((_, rest)) = value.split_once("://") else {
        return false;
    };
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    if authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| !userinfo.is_empty())
    {
        return true;
    }
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let path = path.split('#').next().unwrap_or("");
    (SECRET_URL_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
        && path.split('/').any(|s| !s.is_empty()))
        || path.contains("/webhooks/")
        || path.split('/').any(is_token_like)
        || query
            .split(['&', '#'])
            .filter_map(|kv| kv.split_once('='))
            .any(|(k, v)| !v.is_empty() && is_sensitive_key(k))
}

pub fn is_secret(name: &str, value: &str) -> bool {
    is_sensitive_key(name) || is_secret_value(value)
}

#[cfg(feature = "scrub_text")]
#[allow(clippy::disallowed_types)]
pub fn redact_result_in_place(value: &mut serde_json::Value) {
    redact_result_walk(value, false);
}

#[cfg(feature = "scrub_text")]
#[allow(clippy::disallowed_types)]
fn redact_result_walk(value: &mut serde_json::Value, secret: bool) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                redact_result_walk(v, secret || is_sensitive_key(k));
            }
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                redact_result_walk(v, secret);
            }
        }
        serde_json::Value::String(s) => {
            if secret || is_secret_value(s) {
                *s = REDACTED.to_string();
            } else {
                let redacted = redact_text(s);
                if redacted != *s {
                    *s = redacted;
                }
            }
        }
        _ => {}
    }
}

#[cfg(feature = "scrub_text")]
#[allow(clippy::disallowed_types)]
pub fn redact_result(mut value: serde_json::Value) -> serde_json::Value {
    redact_result_in_place(&mut value);
    value
}

pub fn redact_values(text: &str, values: &[&str]) -> String {
    let out = values
        .iter()
        .filter(|v| !v.is_empty())
        .fold(text.to_string(), |acc, v| acc.replace(v, REDACTED));
    #[cfg(feature = "scrub_text")]
    let out = scrub_line(&out).into_owned();
    out
}

#[cfg(feature = "scrub_text")]
mod text {
    use super::{Cow, REDACTED, SECRET_URL_HOSTS, is_sensitive_key};
    use regex::{Captures, Regex};
    use std::sync::LazyLock;

    struct Rule {
        re: Regex,
        guard: Option<fn(&Captures<'_>) -> bool>,
    }

    fn rule(pattern: &str) -> Rule {
        Rule {
            re: Regex::new(pattern).unwrap(),
            guard: None,
        }
    }

    fn sensitive_key(caps: &Captures<'_>) -> bool {
        caps.name("key")
            .is_some_and(|key| is_sensitive_key(key.as_str()))
    }

    fn keyed(pattern: &str) -> Rule {
        Rule {
            re: Regex::new(pattern).unwrap(),
            guard: Some(sensitive_key),
        }
    }

    static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
        let hosts = SECRET_URL_HOSTS
            .iter()
            .map(|h| regex::escape(h))
            .collect::<Vec<_>>()
            .join("|");
        vec![
            rule(
                r##"-----BEGIN [A-Z0-9 ]*PRIVATE KEY[A-Z0-9 ]*-----(?:.*?-----END [A-Z0-9 ]*PRIVATE KEY[A-Z0-9 ]*-----|.*)"##,
            ),
            rule(r##"(?P<pre>PVEAPIToken=)(?P<sec>[^\s"']+)"##),
            rule(
                r##"(?i)(?P<pre>authorization\s*[:=]\s*"?)(?P<sec>(?:[A-Za-z]+\s+)?[A-Za-z0-9._\-+/=]+)"##,
            ),
            rule(
                r##"(?i)(?P<pre>x-(?:api|auth)-(?:key|token)\s*[:=]\s*"?)(?P<sec>[A-Za-z0-9._\-]+)"##,
            ),
            rule(
                r##"\b(?P<sec>gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_\-]{20,}|(?:AKIA|ASIA)[0-9A-Z]{16}|xox[abprs]-[A-Za-z0-9\-]{10,}|eyJ[A-Za-z0-9_\-]{4,}\.[A-Za-z0-9_\-]{4,}\.[A-Za-z0-9_\-]*)"##,
            ),
            rule(r##"(?P<pre>://)(?P<sec>[^\s/?#@"'<>]+)(?P<post>@)"##),
            rule(
                r##"(?m)(?P<pre>^|[\s"'=(\[,;])(?P<sec>[A-Za-z0-9._\-]+:[^\s:/@"'<>]+)(?P<post>@[A-Za-z0-9])"##,
            ),
            rule(&format!(
                r##"(?P<pre>https?://(?:[A-Za-z0-9\-]+\.)*(?:{hosts})/)(?P<sec>[^\s"'<>]+)"##
            )),
            rule(r##"(?P<pre>/webhooks/)(?P<sec>[^\s"'<>?#]+)"##),
            keyed(
                r##"(?P<pre>"(?P<key>[A-Za-z0-9_.\-]+)"\s*:\s*")(?P<sec>(?:[^"\\]|\\.)*)(?P<post>")"##,
            ),
            keyed(
                r##"(?P<pre>(?P<key>[A-Za-z_][A-Za-z0-9_.\-]*)[ \t]*=[ \t]*)(?P<sec>"[^"]*"|'[^']*'|(?:[^\s"',;&%]|%(?:[^2]|2[^6]))+)"##,
            ),
            keyed(
                r##"(?m)(?P<pre>(?:^|[\s"'])--?(?P<key>[A-Za-z][A-Za-z0-9_.\-]*)[ \t]+)(?P<sec>"[^"]*"|'[^']*'|[^\s"',;\-][^\s"',;]*)"##,
            ),
            keyed(
                r##"(?P<pre>(?P<key>[A-Za-z_][A-Za-z0-9_.\-]*)[ \t]*:[ \t]+)(?P<sec>"[^"]*"|'[^']*'|[^\s"',;]+)"##,
            ),
        ]
    });

    static PEM_OPEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY[A-Z0-9 ]*-----").unwrap());

    fn apply<'a>(rule: &Rule, text: Cow<'a, str>) -> Cow<'a, str> {
        let source: &str = &text;
        let mut out: Option<String> = None;
        let mut last = 0;
        for caps in rule.re.captures_iter(source) {
            let span = caps.name("sec").or_else(|| caps.get(0)).unwrap();
            if span.as_str() == REDACTED {
                continue;
            }
            if let Some(guard) = rule.guard
                && !guard(&caps)
            {
                continue;
            }
            let buf = out.get_or_insert_with(|| String::with_capacity(source.len()));
            buf.push_str(&source[last..span.start()]);
            buf.push_str(REDACTED);
            last = span.end();
        }
        match out {
            Some(mut buf) => {
                buf.push_str(&source[last..]);
                Cow::Owned(buf)
            }
            None => text,
        }
    }

    pub fn scrub_line(line: &str) -> Cow<'_, str> {
        RULES
            .iter()
            .fold(Cow::Borrowed(line), |text, rule| apply(rule, text))
    }

    fn opens_pem_block(line: &str) -> bool {
        PEM_OPEN
            .find_iter(line)
            .last()
            .is_some_and(|open| !line[open.end()..].contains("-----END"))
    }

    pub fn redact_text(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_pem = false;
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            let ending = &line[body.len()..];
            if in_pem {
                in_pem = !body.contains("-----END");
                out.push_str(REDACTED);
            } else {
                in_pem = opens_pem_block(body);
                out.push_str(&scrub_line(body));
            }
            out.push_str(ending);
        }
        out
    }
}

#[cfg(feature = "scrub_text")]
pub use text::{redact_text, scrub_line};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_values_replaces_every_occurrence_and_skips_empty() {
        assert_eq!(
            redact_values("bad hunter2 then hunter2", &["hunter2", ""]),
            format!("bad {REDACTED} then {REDACTED}")
        );
        assert_eq!(redact_values("clean", &[]), "clean");
    }

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
            "PLEX_CLAIM",
            "plex_claim_token",
            "DBPASS",
            "requirepass",
            "MYSQL_ROOT_PASSWORD",
            "x-auth",
            "AUTH",
            "auth",
            "authorization",
        ] {
            assert!(is_sensitive_key(k), "{k} must be treated as sensitive");
        }
    }

    #[test]
    fn identifier_names_are_not_redacted() {
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
            "hasKey",
            "claims",
            "author",
            "authors",
            "bypass",
            "passed",
            "compass",
            "port",
            "no-auth",
            "disable-auth",
            "disabled_auth",
            "without-password",
            "skip-password",
            "enable-auth",
            "enabled_auth",
            "allow-token",
            "use-api-key",
        ] {
            assert!(!is_sensitive_key(k), "{k} must stay visible");
        }
    }

    #[test]
    fn one_vocabulary_covers_env_style_names() {
        for name in [
            "GITHUB_TOKEN",
            "client_secret",
            "DB_PASSWORD",
            "MYSQL_PASSWD",
            "PGPWD",
            "ADMIN_PW",
            "DB_PASS",
            "api_key",
            "PASS",
            "AWS_CREDS",
            "Passphrase",
            "basicauth.users",
            "JWT",
            "PASSWORD_SALT",
            "cookie",
            "HMAC",
        ] {
            assert!(is_sensitive_key(name), "{name}");
        }
        for name in ["TZ", "PUID", "HOME", "PATH", "hostname", "image"] {
            assert!(!is_sensitive_key(name), "{name}");
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

    #[test]
    fn secret_values_are_uris_with_userinfo_or_token_paths() {
        for v in [
            "/run/secrets/db",
            "/var/run/secrets/kubernetes.io/token",
            "postgres://app:hunter2@db:5432/app",
            "https://u:p@host/path?x=1",
            "https://ghp_abc@github.com/o/r.git",
            "https://discord.com/api/webhooks/1234/abcDEF",
            "https://hooks.slack.com/services/T000/B000/XXXX",
            "https://ntfy.sh/alerts",
            "https://ntfy.example/alerts_9f8e7d6c5b4a3a2b",
            "https://example/hook?token=abc",
        ] {
            assert!(is_secret_value(v), "{v}");
        }
        for v in [
            "https://host/a@b",
            "/home/app",
            "redis://cache:6379",
            "https://example/api/v1/items",
            "plain words with https://u:p@host inside",
            "https://example/hook?page=2",
            "/etc/pam.d/common-password",
            "/usr/share/keyrings/debian-archive-keyring.gpg",
            "/home/u/.ssh/authorized_keys",
        ] {
            assert!(!is_secret_value(v), "{v}");
        }
    }

    #[cfg(feature = "scrub_text")]
    #[test]
    fn results_redact_secret_keys_and_values_but_keep_structure() {
        #[allow(clippy::disallowed_types)]
        let out = redact_result(json!({
            "name": "plex",
            "apiKey": "CANARY",
            "nested": [{ "url": "https://u:CANARY@h/x" }, { "dbPassword": "CANARY" }],
            "sessionToken": null,
            "count": 3,
        }));
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("CANARY"), "{text}");
        assert_eq!(out["name"], "plex");
        assert_eq!(out["apiKey"], REDACTED);
        assert_eq!(out["nested"][0]["url"], REDACTED);
        assert!(out["sessionToken"].is_null());
        assert_eq!(out["count"], 3);
    }

    #[cfg(feature = "scrub_text")]
    #[test]
    fn results_replace_only_strings_under_secret_keys() {
        #[allow(clippy::disallowed_types)]
        let out = redact_result(json!({
            "tokenCount": 3,
            "secretReady": true,
            "passwords": { "primary": "CANARY", "rotations": 4, "flags": [true, "CANARY"] },
            "credentials": ["CANARY", { "id": "CANARY" }],
        }));
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("CANARY"), "{text}");
        assert_eq!(out["tokenCount"], 3);
        assert_eq!(out["secretReady"], true);
        assert_eq!(out["passwords"]["primary"], REDACTED);
        assert_eq!(out["passwords"]["rotations"], 4);
        assert_eq!(out["passwords"]["flags"][0], true);
        assert_eq!(out["passwords"]["flags"][1], REDACTED);
        assert!(out["credentials"].is_array());
        assert_eq!(out["credentials"][1]["id"], REDACTED);
    }

    #[cfg(feature = "scrub_text")]
    #[test]
    fn mesh_topology_shaped_result_round_trips() {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            id: String,
            claims: Vec<String>,
            has_key: bool,
            author: String,
            authors: Vec<String>,
            bypass: String,
            passed: bool,
            peers: u32,
            api_key: String,
        }

        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Topology {
            nodes: Vec<Node>,
        }

        #[allow(clippy::disallowed_types)]
        let redacted = redact_result(json!({
            "nodes": [{
                "id": "0a4f6c2e",
                "claims": ["role:admin", "plugin:media"],
                "hasKey": true,
                "author": "scott",
                "authors": ["scott", "orca"],
                "bypass": "none",
                "passed": true,
                "peers": 3,
                "apiKey": "CANARY",
            }]
        }));
        let topology: Topology = serde_json::from_value(redacted).unwrap();
        let node = &topology.nodes[0];
        assert_eq!(node.claims, ["role:admin", "plugin:media"]);
        assert!(node.has_key && node.passed);
        assert_eq!(
            (node.author.as_str(), node.bypass.as_str()),
            ("scott", "none")
        );
        assert_eq!(node.authors, ["scott", "orca"]);
        assert_eq!(node.peers, 3);
        assert_eq!(node.api_key, REDACTED);
    }

    #[cfg(feature = "scrub_text")]
    #[test]
    fn multi_line_text_is_scrubbed_but_keeps_its_shape() {
        let text = "ID=debian\nDB_PASSWORD=hunter2 PUID=1000\nconnect to postgres://app:pw@db:5432/x now\nAuthorization: Bearer abc.def\nrun --token tok123 --port 80\n(see \"https://hooks.slack.com/services/T0/B0/XX\")\nVersion: 3.0.15\n";
        let out = redact_text(text);
        for leak in ["hunter2", "pw@", "abc.def", "tok123", "T0/B0"] {
            assert!(!out.contains(leak), "{leak} in {out}");
        }
        for kept in [
            "ID=debian",
            "PUID=1000",
            "--port 80",
            "Version: 3.0.15",
            "connect to",
            "now",
            "@db:5432/x",
        ] {
            assert!(out.contains(kept), "{kept} missing in {out}");
        }
        assert!(out.contains("DB_PASSWORD=***"), "{out}");
        assert!(out.contains("Authorization: ***"), "{out}");
        assert!(out.contains("postgres://***@db"), "{out}");
        assert_eq!(out.lines().count(), text.lines().count());
    }

    #[cfg(feature = "scrub_text")]
    #[test]
    fn results_scrub_secrets_inside_strings() {
        #[allow(clippy::disallowed_types)]
        let out = redact_result(json!({
            "stdout": "a\nAPI_KEY=k1\nhttps://u:CANARY@h/x\n",
            "plain": "Package: openssl\nVersion: 3.0.15\n",
        }));
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("k1") && !text.contains("CANARY"), "{text}");
        assert_eq!(out["plain"], "Package: openssl\nVersion: 3.0.15\n");
    }

    #[cfg(feature = "scrub_text")]
    #[test]
    fn text_scrub_shares_the_structured_key_list() {
        let line = r#"{"event":"x","clientSecret":"CANARY","dataValue":"keepme"}"#;
        let out = scrub_line(line);
        assert!(!out.contains("CANARY"), "{out}");
        assert!(out.contains("keepme"), "control field was redacted: {out}");
    }

    #[cfg(feature = "scrub_text")]
    mod text_rules {
        use super::*;

        fn assert_scrubbed(input: &str, leak: &str) -> String {
            let out = redact_text(input);
            assert!(!out.contains(leak), "{leak} survived in {out}");
            out
        }

        #[test]
        fn quoted_key_json_pairs() {
            let out = assert_scrubbed(r#"{"password": "hunter2","api_key":"x1y2"}"#, "hunter2");
            assert!(!out.contains("x1y2"), "{out}");
            assert_eq!(out, r#"{"password": "***","api_key":"***"}"#);
        }

        #[test]
        fn json_pair_values_with_escaped_quotes() {
            let out = assert_scrubbed(r#"{"token":"ab\"cd","n":1}"#, "cd");
            assert_eq!(out, r#"{"token":"***","n":1}"#);
        }

        #[test]
        fn percent_encoded_ampersand_terminates_the_value() {
            // A literal `&` stops the bare key=value match (the `[^...&]`
            // exclusion), but its percent-encoded form `%26` is three plain
            // characters to the regex — without also stopping at a bare `%`,
            // the greedy value run swallows past it into the next field.
            let out = assert_scrubbed("token=ab%2Fcd%3Def%26next=1", "cd");
            assert!(!out.contains("ef"), "{out}");
            assert!(out.contains("%26next=1"), "{out}");
        }

        #[test]
        fn a_bare_percent_that_is_not_an_encoded_ampersand_does_not_truncate_the_value() {
            // Only %26 (encoded `&`) should stop the match early; any other
            // `%` — hex-looking or not — is part of the secret and must still
            // be fully redacted.
            for (line, leak) in [
                ("password=P@ss%word!", "word!"),
                ("token=50%offcoupon", "offcoupon"),
                ("token=abc%z", "z"),
                ("token=abc%2z", "2z"),
            ] {
                let out = assert_scrubbed(line, leak);
                assert!(!out.contains('%'), "{out}");
            }
        }

        #[test]
        fn key_equals_value_with_spaces() {
            let out = assert_scrubbed("password = hunter2\nname = web", "hunter2");
            assert_eq!(out, "password = ***\nname = web");
        }

        #[test]
        fn exported_quoted_values_with_spaces() {
            let out = assert_scrubbed("export TOKEN='abc def'\nexport HOME=/root", "abc def");
            assert_eq!(out, "export TOKEN=***\nexport HOME=/root");
            let out = assert_scrubbed(r#"DB_SECRET="two words""#, "two words");
            assert_eq!(out, "DB_SECRET=***");
        }

        #[test]
        fn authorization_equals_bearer() {
            let out = assert_scrubbed("Authorization=Bearer abc.def trailing", "abc.def");
            assert!(!out.contains("Bearer"), "{out}");
            assert!(out.ends_with("trailing"), "{out}");
        }

        #[test]
        fn key_colon_value() {
            let out = assert_scrubbed("password: hunter2 next", "hunter2");
            assert_eq!(out, "password: *** next");
        }

        #[test]
        fn pem_blocks_across_lines() {
            let text = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nabcdef==\n-----END RSA PRIVATE KEY-----\nafter\n";
            let out = assert_scrubbed(text, "MIIEow");
            assert!(!out.contains("abcdef=="), "{out}");
            assert!(
                out.starts_with("before\n") && out.ends_with("\nafter\n"),
                "{out}"
            );
            assert_eq!(out.lines().count(), text.lines().count());
        }

        #[test]
        fn pem_block_on_one_line_and_certificates_untouched() {
            let out = assert_scrubbed(
                r#"{"k":"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADAN\n-----END PRIVATE KEY-----","n":1}"#,
                "MIIEvQ",
            );
            assert!(out.ends_with(r#"","n":1}"#), "{out}");
            let cert = "-----BEGIN CERTIFICATE-----\nMIIDdzCC\n-----END CERTIFICATE-----\n";
            assert_eq!(redact_text(cert), cert);
        }

        #[test]
        fn well_known_token_shapes() {
            for token in [
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "github_pat_11ABCDEFG0abcdefghijkl_mnopqrstuvwxyz",
                "sk-proj-abcdefghijklmnopqrstuvwx",
                "AKIAABCDEFGHIJKLMNOP",
                "xoxb-1234567890-abcdefghij",
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk",
            ] {
                let out = assert_scrubbed(&format!("saw {token} in output"), token);
                assert_eq!(out, "saw *** in output");
            }
            assert_eq!(
                redact_text("task-abcdefghijklmnopqrstuvwx"),
                "task-abcdefghijklmnopqrstuvwx"
            );
        }

        #[test]
        fn userinfo_without_scheme() {
            let out = assert_scrubbed("clone user:s3cr3t@git.example.com/repo now", "s3cr3t");
            assert_eq!(out, "clone ***@git.example.com/repo now");
            assert_eq!(redact_text("mail bob@example.com"), "mail bob@example.com");
        }

        #[test]
        fn json_embedded_name_value_pairs() {
            let out = assert_scrubbed(r#"["DB_PASSWORD=hunter2","TZ=UTC"]"#, "hunter2");
            assert_eq!(out, r#"["DB_PASSWORD=***","TZ=UTC"]"#);
        }

        #[test]
        fn redacts_only_the_matched_span_so_json_stays_valid() {
            let out = assert_scrubbed(r#"{"a":"b","url":"https://u:p@h/x"}"#, "u:p");
            #[allow(clippy::disallowed_types)]
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(parsed["a"], "b");
            assert_eq!(parsed["url"], "https://***@h/x");
        }

        #[test]
        fn flags_do_not_swallow_the_next_flag() {
            assert_eq!(redact_text("--no-auth --port 80"), "--no-auth --port 80");
            assert_eq!(redact_text("--auth --port 80"), "--auth --port 80");
            assert_eq!(
                redact_text("run --token tok123 --port 80"),
                "run --token *** --port 80"
            );
            assert_eq!(
                redact_text("--api-key=k9 --port 80"),
                "--api-key=*** --port 80"
            );
        }

        #[test]
        fn secret_looking_paths_are_not_redacted_in_text_or_results() {
            for path in [
                "/etc/pam.d/common-password",
                "/usr/share/keyrings/debian-archive-keyring.gpg",
                "/home/u/.ssh/authorized_keys",
            ] {
                assert_eq!(redact_text(path), path);
            }
            #[allow(clippy::disallowed_types)]
            let out = redact_result(json!({
                "files": ["/etc/pam.d/common-password", "/home/u/.ssh/authorized_keys"],
                "mount": "/run/secrets/db",
            }));
            assert_eq!(out["files"][0], "/etc/pam.d/common-password");
            assert_eq!(out["files"][1], "/home/u/.ssh/authorized_keys");
            assert_eq!(out["mount"], REDACTED);
        }

        #[test]
        fn clean_lines_stay_borrowed() {
            assert!(matches!(
                scrub_line(r#"{"event":"tick","count":42} a=b version: 3"#),
                std::borrow::Cow::Borrowed(_)
            ));
        }
    }
}
