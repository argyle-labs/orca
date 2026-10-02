//! Pure state for the log viewer: parsing, filtering, selection.
//!
//! Kept free of ratatui and of any terminal so the rules that decide WHAT the
//! operator sees are testable without a tty. The render half owns only layout.

use std::collections::BTreeMap;

/// Severities, ordered most to least severe. The order is the display order of
/// the level toggles and the meaning of "minimum level".
pub const LEVELS: [&str; 5] = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

/// One log line, parsed as far as it will go.
///
/// `raw` is always the original bytes of the line. Everything else is
/// best-effort: the daemon writes JSON, but a panic message or a stray
/// `println!` lands in the same file, and a viewer that drops what it cannot
/// parse hides exactly the lines an operator is hunting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub raw: String,
    pub timestamp: Option<String>,
    pub level: Option<String>,
    pub target: Option<String>,
    pub message: String,
    /// Remaining top-level JSON fields, for the detail pane. Ordered so the
    /// pane is stable between frames rather than reshuffling per render.
    pub fields: BTreeMap<String, String>,
}

impl Record {
    /// Parse one line. Never fails: an unparseable line becomes a `Record`
    /// whose `message` is the whole line and whose level is `None`.
    pub fn parse(raw: &str) -> Record {
        let raw = raw.trim_end_matches(['\n', '\r']).to_string();
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&raw) else {
            return Record {
                message: raw.clone(),
                raw,
                timestamp: None,
                level: None,
                target: None,
                fields: BTreeMap::new(),
            };
        };
        let take = |k: &str| map.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let mut fields = BTreeMap::new();
        for (k, v) in &map {
            if matches!(k.as_str(), "timestamp" | "level" | "target" | "message") {
                continue;
            }
            // Strings unquoted; everything else compact JSON. A nested span
            // object is more useful rendered than elided.
            let rendered = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            fields.insert(k.clone(), rendered);
        }
        Record {
            timestamp: take("timestamp"),
            level: take("level").map(|l| l.to_ascii_uppercase()),
            target: take("target"),
            message: take("message").unwrap_or_default(),
            fields,
            raw,
        }
    }

    /// `HH:MM:SS` out of an RFC3339 timestamp, for the narrow time column.
    /// Falls back to whatever is there when it does not look like RFC3339.
    pub fn short_time(&self) -> &str {
        match &self.timestamp {
            Some(t) if t.len() >= 19 && t.as_bytes()[10] == b'T' => &t[11..19],
            Some(t) => t,
            None => "",
        }
    }
}

/// Which lines the operator currently wants to see.
///
/// Levels and targets are explicit allow-sets rather than a minimum-severity
/// scalar: the question "show me only the mesh warnings" is a two-axis filter,
/// and a single threshold cannot express it.
#[derive(Debug, Clone)]
pub struct Filters {
    /// Enabled severities. A record with no parseable level is governed by
    /// `show_unlevelled`, not by this set.
    pub levels: BTreeMap<String, bool>,
    /// Disabled targets. Absent = enabled, so a target first seen mid-stream
    /// shows up rather than being silently filtered out.
    pub muted_targets: BTreeMap<String, bool>,
    /// Case-insensitive substring over the raw line. Empty = no search.
    pub search: String,
    /// Whether lines that parsed no level are kept. Default true: panics and
    /// raw stderr have no level, and they are the most important lines in the
    /// file.
    pub show_unlevelled: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            levels: LEVELS.iter().map(|l| ((*l).to_string(), true)).collect(),
            muted_targets: BTreeMap::new(),
            search: String::new(),
            show_unlevelled: true,
        }
    }
}

impl Filters {
    pub fn level_enabled(&self, level: &str) -> bool {
        self.levels.get(level).copied().unwrap_or(true)
    }

    pub fn toggle_level(&mut self, level: &str) {
        let e = self.levels.entry(level.to_string()).or_insert(true);
        *e = !*e;
    }

    pub fn target_enabled(&self, target: &str) -> bool {
        !self.muted_targets.get(target).copied().unwrap_or(false)
    }

    pub fn toggle_target(&mut self, target: &str) {
        let e = self
            .muted_targets
            .entry(target.to_string())
            .or_insert(false);
        *e = !*e;
    }

    /// Turn every level back on, unmute every target, clear the search.
    /// One key back to "show me everything" — a viewer you can filter into a
    /// corner with no way out is worse than no filter.
    pub fn reset(&mut self) {
        *self = Filters::default();
    }

    pub fn matches(&self, r: &Record) -> bool {
        match &r.level {
            Some(l) if !self.level_enabled(l) => return false,
            None if !self.show_unlevelled => return false,
            _ => {}
        }
        if let Some(t) = &r.target
            && !self.target_enabled(t)
        {
            return false;
        }
        if !self.search.is_empty() && !r.raw.to_lowercase().contains(&self.search.to_lowercase()) {
            return false;
        }
        true
    }

    /// True when anything is filtered out — drives the "FILTERED" marker, so
    /// an operator never mistakes a filtered view for a quiet system.
    pub fn is_narrowed(&self) -> bool {
        !self.search.is_empty()
            || !self.show_unlevelled
            || self.levels.values().any(|v| !v)
            || self.muted_targets.values().any(|v| *v)
    }
}

/// Indices of the records that pass, in order.
pub fn visible(records: &[Record], f: &Filters) -> Vec<usize> {
    records
        .iter()
        .enumerate()
        .filter(|(_, r)| f.matches(r))
        .map(|(i, _)| i)
        .collect()
}

/// Every target seen, with its line count — the menu the `t` panel renders.
/// Sorted by count descending so the noisy targets an operator wants to mute
/// are at the top, where they can be reached without scrolling.
pub fn targets_by_volume(records: &[Record]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in records {
        if let Some(t) = &r.target {
            *counts.entry(t.clone()).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Clamp a selection into a list of `len` items, keeping it on the last item
/// when the list shrinks underneath it.
pub fn clamp_selection(sel: usize, len: usize) -> usize {
    if len == 0 { 0 } else { sel.min(len - 1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = r#"{"timestamp":"2026-10-02T03:08:35.460640Z","level":"WARN","message":"cookie session present but try_session_auth returned None","path":"/api/mcp/catalog","target":"orca::serve::middleware","span":{"name":"request"}}"#;

    #[test]
    fn a_real_daemon_line_parses_into_columns() {
        let r = Record::parse(REAL);
        assert_eq!(r.short_time(), "03:08:35");
        assert_eq!(r.level.as_deref(), Some("WARN"));
        assert_eq!(r.target.as_deref(), Some("orca::serve::middleware"));
        assert!(r.message.starts_with("cookie session present"));
        // Extra fields survive for the detail pane, including the nested span.
        assert_eq!(r.fields.get("path").unwrap(), "/api/mcp/catalog");
        assert!(r.fields.get("span").unwrap().contains("request"));
    }

    #[test]
    fn a_line_that_is_not_json_is_kept_whole_not_dropped() {
        // A panic does not arrive as JSON, and it is the line that matters most.
        let r = Record::parse("thread 'main' panicked at src/lib.rs:42");
        assert_eq!(r.message, "thread 'main' panicked at src/lib.rs:42");
        assert_eq!(r.level, None);
        assert_eq!(r.short_time(), "");
    }

    #[test]
    fn unlevelled_lines_are_visible_by_default() {
        let recs = vec![Record::parse("thread 'main' panicked")];
        assert_eq!(visible(&recs, &Filters::default()), vec![0]);
    }

    #[test]
    fn muting_a_level_hides_only_that_level() {
        let recs = vec![
            Record::parse(r#"{"level":"INFO","message":"a"}"#),
            Record::parse(r#"{"level":"WARN","message":"b"}"#),
        ];
        let mut f = Filters::default();
        f.toggle_level("INFO");
        assert_eq!(visible(&recs, &f), vec![1]);
        assert!(f.is_narrowed());
    }

    #[test]
    fn a_target_unseen_at_startup_is_visible_when_it_first_appears() {
        // Muting is an explicit deny-set precisely so a target that only shows
        // up once the daemon reaches some code path is not invisible.
        let f = Filters::default();
        let r = Record::parse(r#"{"level":"INFO","target":"orca::brand::new","message":"x"}"#);
        assert!(f.matches(&r));
    }

    #[test]
    fn muting_a_target_hides_it_and_reset_brings_it_back() {
        let recs = vec![
            Record::parse(r#"{"level":"INFO","target":"orca::serve::middleware","message":"a"}"#),
            Record::parse(r#"{"level":"INFO","target":"orca::mesh","message":"b"}"#),
        ];
        let mut f = Filters::default();
        f.toggle_target("orca::serve::middleware");
        assert_eq!(visible(&recs, &f), vec![1]);
        f.reset();
        assert_eq!(visible(&recs, &f), vec![0, 1]);
        assert!(!f.is_narrowed());
    }

    #[test]
    fn search_is_case_insensitive_and_spans_the_whole_raw_line() {
        let recs = vec![Record::parse(REAL)];
        // Matches a field value, not just the message — an operator searching
        // for a path or a correlation id expects it to hit.
        let mut f = Filters {
            search: "API/MCP".into(),
            ..Default::default()
        };
        assert_eq!(visible(&recs, &f), vec![0]);
        f.search = "nothing-here".into();
        assert!(visible(&recs, &f).is_empty());
    }

    #[test]
    fn targets_are_offered_noisiest_first() {
        let mut recs = vec![Record::parse(r#"{"target":"orca::mesh","message":"m"}"#)];
        for _ in 0..3 {
            recs.push(Record::parse(
                r#"{"target":"orca::serve::middleware","message":"x"}"#,
            ));
        }
        let t = targets_by_volume(&recs);
        assert_eq!(t[0], ("orca::serve::middleware".to_string(), 3));
        assert_eq!(t[1], ("orca::mesh".to_string(), 1));
    }

    #[test]
    fn selection_rides_the_end_of_a_shrinking_list() {
        assert_eq!(clamp_selection(9, 3), 2);
        assert_eq!(clamp_selection(9, 0), 0);
        assert_eq!(clamp_selection(1, 3), 1);
    }

    #[test]
    fn a_default_filter_hides_nothing() {
        let f = Filters::default();
        assert!(!f.is_narrowed(), "a fresh viewer must show the whole file");
        for lvl in LEVELS {
            assert!(f.level_enabled(lvl), "{lvl}");
        }
    }
}
