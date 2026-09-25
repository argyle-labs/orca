//! Detects config-parse failures that a service *already logged*.
//!
//! [`super::config_format`] is the write-side guard: orca validates before it
//! writes, so orca can never be the thing that corrupts a managed config. That
//! only covers files orca wrote. The frigg Jellyfin outage was the other half —
//! an over-eager `sed` stripped the attribute quotes from
//! `/etc/jellyfin/network.xml`, and nothing wrote that file through orca, so no
//! write-side check could ever have fired.
//!
//! What DID happen is that Jellyfin logged the parse failure, fell back to
//! defaults, and kept reporting healthy — for six months. The evidence was
//! sitting in a log file the whole time and nobody was reading it. This module
//! is the reader: give it log text, get back typed findings naming the file and
//! the format that failed.
//!
//! Deliberately a pure text function, not a log *collector*. Services keep their
//! logs in wildly different places (journald, a container's stdout, a file
//! inside a guest), so the source is the caller's problem — a diagnostics
//! provider, `guest_exec`, a plugin. Keeping the detector pure is what makes it
//! testable against real log lines instead of a live service.
//!
//! False positives matter here: this feeds an operator-facing finding, so a rule
//! only earns its place if it cannot plausibly match a healthy log line. Matching
//! on a bare `"error"` would flag every transient HTTP failure; every signature
//! below names a *parser* failing on a *document*.

use super::config_format::ConfigFormat;

/// One config-parse failure found in log text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailure {
    /// Stable rule id that matched, for dedupe and for naming the check in a
    /// finding (e.g. `"dotnet-xml"`). Not operator-facing prose.
    pub signature: &'static str,
    /// The config format the failing parser was reading, when the signature
    /// implies one. `None` for format-agnostic messages like
    /// "failed to parse config".
    pub format: Option<ConfigFormat>,
    /// Config file named on the same line, when there is one. Frequently absent
    /// — plenty of loggers report the parse error without the path, which is
    /// exactly why [`ParseFailure::line`] is retained as evidence.
    pub path: Option<String>,
    /// The matching line, trimmed. The operator needs the parser's own message
    /// (line/column, offending token), not a summary.
    pub line: String,
}

/// A detection rule: a needle that must appear (case-insensitively) plus the
/// format it implicates.
struct Rule {
    signature: &'static str,
    needle: &'static str,
    format: Option<ConfigFormat>,
}

/// Ordered most-specific first, so the reported `signature` is the most
/// informative one when a line trips several rules.
const RULES: &[Rule] = &[
    // ── XML ──────────────────────────────────────────────────────────────────
    // .NET / Jellyfin / Sonarr / Radarr / Prowlarr — the whole *arr stack plus
    // Emby-derived servers. This is the exact family that produced the frigg
    // outage; `Error loading configuration file` is what Jellyfin actually wrote.
    Rule {
        signature: "dotnet-xml",
        needle: "there is an error in xml document",
        format: Some(ConfigFormat::Xml),
    },
    Rule {
        signature: "dotnet-config-load",
        needle: "error loading configuration file",
        format: Some(ConfigFormat::Xml),
    },
    Rule {
        signature: "xml-exception",
        needle: "xmlexception",
        format: Some(ConfigFormat::Xml),
    },
    // libxml2 / Python lxml / PHP — "opening and ending tag mismatch" and
    // friends all carry this prefix.
    Rule {
        signature: "libxml-parser",
        needle: "xml parser error",
        format: Some(ConfigFormat::Xml),
    },
    Rule {
        signature: "xml-not-well-formed",
        needle: "not well-formed",
        format: Some(ConfigFormat::Xml),
    },
    // ── YAML ─────────────────────────────────────────────────────────────────
    // PyYAML (Home Assistant, Bazarr, many Python services).
    Rule {
        signature: "pyyaml-scanner",
        needle: "yaml.scanner.scannererror",
        format: Some(ConfigFormat::Yaml),
    },
    Rule {
        signature: "pyyaml-parser",
        needle: "yaml.parser.parsererror",
        format: Some(ConfigFormat::Yaml),
    },
    // The two most common YAML hand-edit mistakes: a tab/indent slip and a
    // missing colon. Both are distinctive enough to be safe needles.
    Rule {
        signature: "yaml-mapping-values",
        needle: "mapping values are not allowed",
        format: Some(ConfigFormat::Yaml),
    },
    Rule {
        signature: "yaml-expected-colon",
        needle: "could not find expected ':'",
        format: Some(ConfigFormat::Yaml),
    },
    Rule {
        signature: "yaml-generic",
        needle: "error parsing yaml",
        format: Some(ConfigFormat::Yaml),
    },
    // ── JSON ─────────────────────────────────────────────────────────────────
    // Python json, Go encoding/json, serde_json, and JS JSON.parse respectively.
    Rule {
        signature: "python-json-decode",
        needle: "jsondecodeerror",
        format: Some(ConfigFormat::Json),
    },
    Rule {
        signature: "go-json-unmarshal",
        needle: "cannot unmarshal",
        format: Some(ConfigFormat::Json),
    },
    Rule {
        signature: "serde-json-expected-value",
        needle: "expected value at line",
        format: Some(ConfigFormat::Json),
    },
    Rule {
        signature: "js-json-parse",
        needle: "unexpected token",
        format: Some(ConfigFormat::Json),
    },
    Rule {
        signature: "json-invalid",
        needle: "invalid json",
        format: Some(ConfigFormat::Json),
    },
    // ── TOML ─────────────────────────────────────────────────────────────────
    Rule {
        signature: "toml-invalid",
        needle: "invalid toml",
        format: Some(ConfigFormat::Toml),
    },
    Rule {
        signature: "toml-expected-equals",
        needle: "expected an equals",
        format: Some(ConfigFormat::Toml),
    },
    // ── Format-agnostic ──────────────────────────────────────────────────────
    // Worth keeping despite naming no format: a service that says this has
    // told us the config is unusable, which is the actionable part. The format
    // is a nicety; the file being ignored is the outage.
    Rule {
        signature: "failed-to-parse-config",
        needle: "failed to parse config",
        format: None,
    },
    Rule {
        signature: "malformed-config",
        needle: "malformed configuration",
        format: None,
    },
    Rule {
        signature: "config-parse-error",
        needle: "error parsing configuration",
        format: None,
    },
];

/// Characters that can legally sit inside a config path we would report. Used to
/// bound path extraction — a log line is prose around a path, so the path ends
/// at the first character prose would use.
fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~' | '+' | '@')
}

/// Pull the first plausible config path out of a line.
///
/// Requires an extension `config_format` recognises, so this returns the file
/// that actually failed rather than the first slash-bearing token (a logger
/// name, a URL path, a namespace). Returns `None` rather than guessing — a
/// finding with no path but a real log line is still actionable; a finding
/// pointing at the wrong file is worse than one pointing at none.
pub fn extract_path(line: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for (i, _) in line.char_indices() {
        // Anchor on separators only, so we start at a token boundary.
        if i > 0 {
            let prev = line[..i].chars().next_back()?;
            if is_path_char(prev) {
                continue;
            }
        }
        let tail = &line[i..];
        let end = tail.find(|c: char| !is_path_char(c)).unwrap_or(tail.len());
        let tok = tail[..end].trim_end_matches('.');
        if tok.len() < 3 {
            continue;
        }
        // Must look like a file of a format we know, and must not be a bare
        // extension-looking word (`config.xml` yes, `.xml` no).
        if super::config_format::from_path(std::path::Path::new(tok)).is_some()
            && !tok.starts_with('.')
            && best.is_none()
        {
            best = Some(tok.to_string());
        }
    }
    best
}

/// Scan log text for config-parse failures, one finding per matching line.
///
/// Lines are independent: a multi-line stack trace yields a finding for the line
/// carrying the parser message, not for each frame. Order follows the input so a
/// caller can show the earliest occurrence — which is the one that says how long
/// the service has been running on a config it could not read.
pub fn scan(log: &str) -> Vec<ParseFailure> {
    let mut out = Vec::new();
    for raw in log.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        // First match wins: RULES is ordered most-specific first, and one log
        // line describes one failure. Without this a .NET XML error would also
        // trip the generic rules and report the same outage three times.
        if let Some(rule) = RULES.iter().find(|r| lower.contains(r.needle)) {
            out.push(ParseFailure {
                signature: rule.signature,
                format: rule.format,
                path: extract_path(line),
                line: line.to_string(),
            });
        }
    }
    out
}

/// Distinct `(signature, path)` pairs from a scan, keeping the first occurrence.
///
/// A service that cannot parse its config logs that on *every* restart and often
/// every reload, so a raw scan of a long log reports the same defect hundreds of
/// times. An operator needs the set of broken files, not the repetition count.
pub fn distinct(failures: &[ParseFailure]) -> Vec<ParseFailure> {
    let mut seen: Vec<(&str, Option<&str>)> = Vec::new();
    let mut out = Vec::new();
    for f in failures {
        let key = (f.signature, f.path.as_deref());
        if !seen.contains(&key) {
            seen.push(key);
            out.push(f.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The actual outage, in the shape Jellyfin logs it. If this ever stops
    /// matching, the six-month-silent-failure class is undetectable again.
    #[test]
    fn detects_the_frigg_jellyfin_xml_failure() {
        let log = "[2026-03-15 04:12:07] [ERR] [1] Emby.Server.Implementations.\
                   AppBase.BaseConfigurationManager: Error loading configuration \
                   file: /etc/jellyfin/network.xml";
        let found = scan(log);
        assert_eq!(found.len(), 1, "expected exactly one finding: {found:?}");
        assert_eq!(found[0].signature, "dotnet-config-load");
        assert_eq!(found[0].format, Some(ConfigFormat::Xml));
        assert_eq!(found[0].path.as_deref(), Some("/etc/jellyfin/network.xml"));
    }

    #[test]
    fn detects_dotnet_xml_document_error_without_a_path() {
        let found =
            scan("System.InvalidOperationException: There is an error in XML document (3, 42).");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].signature, "dotnet-xml");
        // No path in the line — the finding is still actionable via `line`.
        assert_eq!(found[0].path, None);
        assert!(found[0].line.contains("(3, 42)"), "evidence is preserved");
    }

    #[test]
    fn detects_each_format_family() {
        let cases = [
            (
                "yaml.scanner.ScannerError: mapping values are not allowed here",
                ConfigFormat::Yaml,
            ),
            (
                "json.decoder.JSONDecodeError: Expecting ',' delimiter: line 4",
                ConfigFormat::Json,
            ),
            ("Error: invalid TOML in config.toml", ConfigFormat::Toml),
            ("XmlException: unexpected end of file", ConfigFormat::Xml),
        ];
        for (line, want) in cases {
            let found = scan(line);
            assert_eq!(found.len(), 1, "no finding for {line:?}");
            assert_eq!(found[0].format, Some(want), "wrong format for {line:?}");
        }
    }

    /// The rules must not fire on ordinary operational noise. This is the test
    /// that keeps the check trustworthy enough to act on.
    #[test]
    fn healthy_and_unrelated_error_lines_do_not_match() {
        let log = "\
[INFO] Loaded configuration from /etc/jellyfin/network.xml
[WARN] HTTP 500 from https://api.example.com/v1/items
[ERROR] Connection refused connecting to 10.0.0.5:8096
[ERROR] Failed to open file /mnt/data/show.mkv: No such file or directory
[INFO] error rate nominal
[ERROR] FFmpeg exited with code 254";
        assert_eq!(scan(log), vec![], "false positive on healthy log");
    }

    #[test]
    fn a_specific_rule_wins_over_a_generic_one_on_the_same_line() {
        // Trips both `dotnet-config-load` and `failed-to-parse-config`; the
        // report must name the informative one, once.
        let found =
            scan("Error loading configuration file: failed to parse config /app/settings.xml");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].signature, "dotnet-config-load");
    }

    #[test]
    fn scan_reports_every_line_but_distinct_collapses_restart_spam() {
        let log = "\
Error loading configuration file: /etc/jellyfin/network.xml
Error loading configuration file: /etc/jellyfin/network.xml
Error loading configuration file: /etc/jellyfin/encoding.xml";
        let all = scan(log);
        assert_eq!(all.len(), 3, "scan is per-line");
        let uniq = distinct(&all);
        assert_eq!(uniq.len(), 2, "two broken files, not three log lines");
        assert_eq!(uniq[0].path.as_deref(), Some("/etc/jellyfin/network.xml"));
        assert_eq!(uniq[1].path.as_deref(), Some("/etc/jellyfin/encoding.xml"));
    }

    #[test]
    fn extract_path_prefers_a_known_config_extension_over_other_tokens() {
        // A logger name and a URL both precede the real file.
        let line = "Emby.Server.Implementations: GET /web/index.html failed; \
                    reloading /etc/jellyfin/system.xml";
        assert_eq!(
            extract_path(line).as_deref(),
            // `.html` is not a config format, so it must be skipped.
            Some("/etc/jellyfin/system.xml")
        );
    }

    #[test]
    fn extract_path_returns_none_rather_than_guessing() {
        assert_eq!(
            extract_path("There is an error in XML document (1, 1)"),
            None
        );
        assert_eq!(extract_path("could not read .xml"), None);
    }

    #[test]
    fn trailing_sentence_punctuation_is_not_part_of_the_path() {
        let found = scan("Error loading configuration file: /etc/app/conf.json.");
        assert_eq!(found[0].path.as_deref(), Some("/etc/app/conf.json"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let found = scan("ERROR LOADING CONFIGURATION FILE: /etc/a/b.XML");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].signature, "dotnet-config-load");
    }

    #[test]
    fn empty_and_blank_input_is_not_a_finding() {
        assert_eq!(scan(""), vec![]);
        assert_eq!(scan("\n\n   \n"), vec![]);
    }
}
