//! Pure reconcile-logic tests — no daemon, no db, no filesystem. Schemas and
//! live rows are injected directly.

// Free-form config-row JSON is modelled as `serde_json::Value` on purpose (see
// the crate's reconcile.rs rationale); the test fixtures build it with `json!`.
#![allow(clippy::disallowed_types)]

use config_source::reconcile::{
    LiveRow, RepoRow, SchemaIndex, compute_diff, parse_host_config, validate_row,
};
use serde_json::json;

fn schema_service() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string" },
            "replicas": { "type": "integer", "minimum": 1 }
        },
        "required": ["name", "kind"]
    })
}

fn index_with_service() -> SchemaIndex {
    let mut idx = SchemaIndex::new();
    idx.insert("service", schema_service());
    idx
}

// ── Parsing ──────────────────────────────────────────────────────────────────

#[test]
fn parse_extracts_rows_grouped_by_noun() {
    let src = r#"
[[service]]
name = "plex"
kind = "container"

[[service]]
name = "sonarr"
kind = "container"

[[nfs_watch]]
name = "data"
export = "10.10.10.10:/mnt/user/data"
"#;
    let rows = parse_host_config("thor", src).expect("parse");
    assert_eq!(rows.len(), 3);
    // Sorted by (noun, name): nfs_watch/data, service/plex, service/sonarr.
    assert_eq!(
        (rows[0].noun.as_str(), rows[0].name.as_str()),
        ("nfs_watch", "data")
    );
    assert_eq!(
        (rows[1].noun.as_str(), rows[1].name.as_str()),
        ("service", "plex")
    );
    assert_eq!(
        (rows[2].noun.as_str(), rows[2].name.as_str()),
        ("service", "sonarr")
    );
    assert!(rows.iter().all(|r| r.host_owner == "thor"));
    // The whole table (name included) becomes the json payload.
    assert_eq!(rows[1].json["kind"], "container");
}

#[test]
fn parse_rejects_row_without_name() {
    let src = r#"
[[service]]
kind = "container"
"#;
    let err = parse_host_config("thor", src).unwrap_err().to_string();
    assert!(err.contains("missing a string `name`"), "got: {err}");
}

#[test]
fn parse_rejects_non_array_noun() {
    let src = r#"
[service]
name = "plex"
"#;
    let err = parse_host_config("thor", src).unwrap_err().to_string();
    assert!(err.contains("array-of-tables"), "got: {err}");
}

// ── Validation ───────────────────────────────────────────────────────────────

#[test]
fn validate_passes_for_conforming_row() {
    let idx = index_with_service();
    let row = RepoRow {
        host_owner: "thor".into(),
        noun: "service".into(),
        name: "plex".into(),
        json: json!({ "name": "plex", "kind": "container", "replicas": 2 }),
    };
    assert!(validate_row(&idx, &row).is_empty());
}

#[test]
fn validate_fails_for_missing_required_and_bad_type() {
    let idx = index_with_service();
    let row = RepoRow {
        host_owner: "thor".into(),
        noun: "service".into(),
        name: "plex".into(),
        json: json!({ "name": "plex", "replicas": 0 }),
    };
    let errors = validate_row(&idx, &row);
    assert!(!errors.is_empty(), "expected violations");
}

#[test]
fn validate_skips_unschematized_noun() {
    // No schema registered for `share` → treated as valid-but-unvalidated.
    let idx = index_with_service();
    let row = RepoRow {
        host_owner: "willow".into(),
        noun: "share".into(),
        name: "data".into(),
        json: json!({ "anything": true }),
    };
    assert!(validate_row(&idx, &row).is_empty());
}

// ── Diff ─────────────────────────────────────────────────────────────────────

fn repo(host: &str, noun: &str, name: &str, json: serde_json::Value) -> RepoRow {
    RepoRow {
        host_owner: host.into(),
        noun: noun.into(),
        name: name.into(),
        json,
    }
}

fn live(host: &str, noun: &str, name: &str, json: serde_json::Value, is_replica: bool) -> LiveRow {
    LiveRow {
        host_owner: host.into(),
        noun: noun.into(),
        name: name.into(),
        json,
        is_replica,
    }
}

#[test]
fn diff_classifies_add_change_delete() {
    let idx = index_with_service();
    let repo_rows = vec![
        repo(
            "thor",
            "service",
            "plex",
            json!({ "name": "plex", "kind": "container" }),
        ),
        repo(
            "thor",
            "service",
            "sonarr",
            json!({ "name": "sonarr", "kind": "container", "replicas": 3 }),
        ),
    ];
    let live_rows = vec![
        // unchanged
        live(
            "thor",
            "service",
            "plex",
            json!({ "name": "plex", "kind": "container" }),
            false,
        ),
        // changed (sonarr replicas differ) — sonarr is in repo, so this is a change
        live(
            "thor",
            "service",
            "sonarr",
            json!({ "name": "sonarr", "kind": "container", "replicas": 1 }),
            false,
        ),
        // in live only → delete
        live(
            "thor",
            "service",
            "radarr",
            json!({ "name": "radarr", "kind": "container" }),
            false,
        ),
    ];
    let plan = compute_diff("thor", &repo_rows, &live_rows, &idx);
    assert!(plan.to_add.is_empty(), "plex+sonarr both exist live");
    assert_eq!(plan.to_change.len(), 1);
    assert_eq!(plan.to_change[0].name, "sonarr");
    assert_eq!(plan.to_delete.len(), 1);
    assert_eq!(plan.to_delete[0].name, "radarr");
    assert!(plan.schema_invalid.is_empty());
}

#[test]
fn diff_reports_add_for_new_repo_row() {
    let idx = index_with_service();
    let repo_rows = vec![repo(
        "thor",
        "service",
        "plex",
        json!({ "name": "plex", "kind": "container" }),
    )];
    let plan = compute_diff("thor", &repo_rows, &[], &idx);
    assert_eq!(plan.to_add.len(), 1);
    assert_eq!(plan.to_add[0].name, "plex");
}

#[test]
fn diff_puts_invalid_rows_in_schema_invalid_and_excludes_them() {
    let idx = index_with_service();
    // Missing required `kind` → invalid → must NOT appear in to_add.
    let repo_rows = vec![repo(
        "thor",
        "service",
        "broken",
        json!({ "name": "broken" }),
    )];
    let plan = compute_diff("thor", &repo_rows, &[], &idx);
    assert!(plan.to_add.is_empty());
    assert_eq!(plan.schema_invalid.len(), 1);
    assert_eq!(plan.schema_invalid[0].name, "broken");
    assert!(!plan.schema_invalid[0].errors.is_empty());
}

#[test]
fn diff_ownership_ignores_other_host_repo_rows() {
    let idx = index_with_service();
    // Repo row owned by frigg must not be planned when reconciling thor.
    let repo_rows = vec![repo(
        "frigg",
        "service",
        "jellyfin",
        json!({ "name": "jellyfin", "kind": "container" }),
    )];
    let plan = compute_diff("thor", &repo_rows, &[], &idx);
    assert!(plan.to_add.is_empty());
    assert!(plan.schema_invalid.is_empty());
}

#[test]
fn diff_never_deletes_replica_rows() {
    let idx = index_with_service();
    // Live replica row with no repo counterpart must NOT be delete-planned.
    let live_rows = vec![live(
        "thor",
        "service",
        "mesh-copy",
        json!({ "name": "mesh-copy", "kind": "container" }),
        true,
    )];
    let plan = compute_diff("thor", &[], &live_rows, &idx);
    assert!(
        plan.to_delete.is_empty(),
        "replicas are never delete-planned"
    );
}

#[test]
fn diff_ignores_other_host_live_rows_for_delete() {
    let idx = index_with_service();
    // A live row owned by frigg is not authoritative for a thor reconcile.
    let live_rows = vec![live(
        "frigg",
        "service",
        "jellyfin",
        json!({ "name": "jellyfin", "kind": "container" }),
        false,
    )];
    let plan = compute_diff("thor", &[], &live_rows, &idx);
    assert!(plan.to_delete.is_empty());
}
