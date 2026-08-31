//! Pure reconcile logic — no daemon, no db, no filesystem.
//!
//! Everything here takes its inputs (repo rows, live rows, schema index) as
//! plain values so it can be unit-tested without a running daemon. The
//! `tools` module is the only place that reaches the live catalog / db.
//!
//! Reconcile direction (tonight): git repo → live config store, READ-ONLY.
//! We compute a plan (`DiffPlan`); we never mutate. Deletes are *reported*,
//! never executed.

// Config-row payloads and per-noun schemas are genuinely free-form upstream —
// their shape varies per noun/plugin and is only known at runtime (the same
// reason `db::config_store` stores them as opaque strings). We model them as
// `serde_json::Value` deliberately. See CLAUDE.md / `db/src/config_store.rs`.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;

use jsonschema::Draft;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A config row parsed out of the meerkat checkout (`config/<host>/*.toml`).
/// `host_owner` is the directory name — the sole ownership authority.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoRow {
    pub host_owner: String,
    pub noun: String,
    pub name: String,
    pub json: Value,
}

/// A row already present in the live config store (`config_list`). `is_replica`
/// marks a mesh replica — never locally git-applied, so never delete-planned.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRow {
    pub host_owner: String,
    pub noun: String,
    pub name: String,
    pub json: Value,
    pub is_replica: bool,
}

/// Per-noun JSON Schemas resolved from the live daemon (the config-store schema
/// registry, populated by each domain as it loads). Injected as a value so the
/// diff/validation logic needs no daemon under test.
#[derive(Debug, Clone, Default)]
pub struct SchemaIndex {
    by_noun: BTreeMap<String, Value>,
}

impl SchemaIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, noun: impl Into<String>, schema: Value) {
        self.by_noun.insert(noun.into(), schema);
    }

    pub fn get(&self, noun: &str) -> Option<&Value> {
        self.by_noun.get(noun)
    }

    pub fn len(&self) -> usize {
        self.by_noun.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_noun.is_empty()
    }
}

// ── Output shapes ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct RowRef {
    pub host_owner: String,
    pub noun: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct SchemaInvalid {
    pub host_owner: String,
    pub noun: String,
    pub name: String,
    /// One human-readable message per schema violation.
    pub errors: Vec<String>,
}

/// The read-only reconcile plan. Nothing here is executed tonight.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq)]
pub struct DiffPlan {
    /// In repo, absent from the live store → would be created.
    pub to_add: Vec<RowRef>,
    /// In both, JSON differs → would be replaced.
    pub to_change: Vec<RowRef>,
    /// Live authoritative rows (owned, non-replica) with no repo counterpart →
    /// REPORTED as removable. Never executed in this slice.
    pub to_delete: Vec<RowRef>,
    /// Repo rows that failed schema validation → excluded from add/change.
    pub schema_invalid: Vec<SchemaInvalid>,
}

// ── TOML parsing ─────────────────────────────────────────────────────────────

/// Parse one `config/<host>/*.toml` document into rows.
///
/// Shape: top-level keys are nouns; each maps to an array-of-tables, one table
/// per row. Every table MUST carry a string `name`. The whole table (name
/// included) becomes the row's `json` payload.
///
/// ```toml
/// [[service]]
/// name = "plex"
/// kind = "container"
///
/// [[nfs_watch]]
/// name = "data"
/// export = "10.10.10.10:/mnt/user/data"
/// ```
pub fn parse_host_config(host_owner: &str, toml_src: &str) -> anyhow::Result<Vec<RepoRow>> {
    let table: toml::Table = toml::from_str(toml_src)?;
    let mut rows = Vec::new();

    for (noun, value) in table {
        let toml::Value::Array(items) = value else {
            anyhow::bail!(
                "config for host `{host_owner}`: noun `{noun}` must be an array-of-tables ([[{noun}]])"
            );
        };
        for (idx, item) in items.into_iter().enumerate() {
            let toml::Value::Table(row_table) = item else {
                anyhow::bail!("config for host `{host_owner}`: `{noun}[{idx}]` must be a table");
            };
            let name = row_table
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "config for host `{host_owner}`: `{noun}[{idx}]` is missing a string `name`"
                    )
                })?
                .to_string();
            let json = toml_table_to_json(row_table)?;
            rows.push(RepoRow {
                host_owner: host_owner.to_string(),
                noun: noun.clone(),
                name,
                json,
            });
        }
    }

    rows.sort_by(|a, b| (a.noun.as_str(), a.name.as_str()).cmp(&(&b.noun, &b.name)));
    Ok(rows)
}

fn toml_table_to_json(table: toml::Table) -> anyhow::Result<Value> {
    // Round-trip through serde_json's data model. TOML values are a strict
    // subset (no null), so this is lossless for our purposes.
    let value = serde_json::to_value(table)?;
    Ok(value)
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Validate a row's JSON against its noun schema (Draft 2020-12 — the dialect
/// the served spec declares). Returns the list of violations; empty = valid.
///
/// A noun with no registered schema is treated as valid-but-unvalidated: we do
/// NOT reject rows we cannot check (that would be the cold-schema failure mode).
/// The `tools` layer asserts daemon-liveness up front so the index is never
/// silently empty.
pub fn validate_row(index: &SchemaIndex, row: &RepoRow) -> Vec<String> {
    let Some(schema) = index.get(&row.noun) else {
        return Vec::new();
    };
    let validator = match jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(schema)
    {
        Ok(v) => v,
        Err(e) => return vec![format!("schema for noun `{}` is invalid: {e}", row.noun)],
    };
    validator
        .iter_errors(&row.json)
        .map(|e| e.to_string())
        .collect()
}

// ── Diff ─────────────────────────────────────────────────────────────────────

/// Compute the read-only reconcile plan for a single host.
///
/// Ownership: only repo rows whose `host_owner == host` and live rows that are
/// owned (`host_owner == host`) AND non-replica are authoritative. Replicas are
/// never delete-planned.
pub fn compute_diff(
    host: &str,
    repo_rows: &[RepoRow],
    live_rows: &[LiveRow],
    index: &SchemaIndex,
) -> DiffPlan {
    let mut plan = DiffPlan::default();

    // Valid, owned repo rows keyed by (noun, name).
    let mut repo_valid: BTreeMap<(String, String), &RepoRow> = BTreeMap::new();
    for row in repo_rows.iter().filter(|r| r.host_owner == host) {
        let errors = validate_row(index, row);
        if !errors.is_empty() {
            plan.schema_invalid.push(SchemaInvalid {
                host_owner: row.host_owner.clone(),
                noun: row.noun.clone(),
                name: row.name.clone(),
                errors,
            });
            continue;
        }
        repo_valid.insert((row.noun.clone(), row.name.clone()), row);
    }

    // Owned, non-replica live rows keyed by (noun, name).
    let live_owned: BTreeMap<(String, String), &LiveRow> = live_rows
        .iter()
        .filter(|r| r.host_owner == host && !r.is_replica)
        .map(|r| ((r.noun.clone(), r.name.clone()), r))
        .collect();

    for (key, row) in &repo_valid {
        match live_owned.get(key) {
            None => plan.to_add.push(row_ref(row)),
            Some(live) if live.json != row.json => plan.to_change.push(row_ref(row)),
            Some(_) => {}
        }
    }

    for (key, live) in &live_owned {
        if !repo_valid.contains_key(key) {
            plan.to_delete.push(RowRef {
                host_owner: live.host_owner.clone(),
                noun: live.noun.clone(),
                name: live.name.clone(),
            });
        }
    }

    plan
}

fn row_ref(row: &RepoRow) -> RowRef {
    RowRef {
        host_owner: row.host_owner.clone(),
        noun: row.noun.clone(),
        name: row.name.clone(),
    }
}
