//! Real-world validation of the 3.1 -> 3.0 lowering pass against the Plex
//! Media Server OpenAPI spec (`openapi: 3.1.1`, MIT licensed).
//!
//! The proof is end-to-end: a genuine 3.1.1 document, parsed from YAML,
//! becomes a value that `openapiv3` (the 3.0 type system progenitor consumes)
//! deserializes successfully, then survives the existing `for_progenitor`
//! normalize pass.
//!
//! The fixture lives at the repo root (`plex-api-spec.yaml`), two levels up
//! from this crate. If it is missing, the test fetches it once and caches it
//! there so the suite is self-sufficient offline thereafter.

use std::path::PathBuf;

const PLEX_SPEC_URL: &str =
    "https://raw.githubusercontent.com/LukeHagar/plex-api-spec/main/plex-api-spec.yaml";

fn fixture_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plex-api-spec.yaml"
    ))
}

fn load_fixture() -> String {
    let path = fixture_path();
    if let Ok(raw) = std::fs::read_to_string(&path) {
        return raw;
    }
    let raw = reqwest::blocking::get(PLEX_SPEC_URL)
        .expect("GET plex spec")
        .error_for_status()
        .expect("plex spec status")
        .text()
        .expect("plex spec body");
    std::fs::write(&path, &raw).expect("cache plex spec");
    raw
}

#[test]
fn plex_31_spec_lowers_and_deserializes_into_openapiv3() {
    let raw = load_fixture();

    // The spec is YAML; lowering operates on the parsed value regardless of
    // the source format.
    #[allow(clippy::disallowed_types)]
    let mut value: serde_json::value::Value =
        serde_yaml::from_str(&raw).expect("parse plex spec YAML");

    assert!(
        openapi::lower_31::is_31(&value),
        "fixture must be an OpenAPI 3.1 document"
    );

    let report = openapi::lower_31::lower_to_30(&mut value)
        .expect("plex 3.1 spec must lower cleanly — if this errors, the spec uses a 3.1 construct with no 3.0 equivalent; investigate, do not weaken the error");

    // Real proof: a 3.1.1 document is now valid `openapiv3`.
    let mut spec: openapiv3::OpenAPI = serde_json::value::from_value(value)
        .expect("lowered plex spec must deserialize into openapiv3");
    assert_eq!(spec.openapi, "3.0.3");

    // And it survives the existing normalize pass without panicking.
    let _ = openapi::normalize::for_progenitor(&mut spec);

    // The lowering should have done real work on a spec this size.
    let total = report.nullable_type_arrays.len()
        + report.examples_to_example.len()
        + report.numeric_exclusive.len()
        + report.const_to_enum.len()
        + report.dropped_schema_keyword.len()
        + report.dropped_content_keywords.len();
    assert!(total > 0, "expected at least one lowering on the Plex spec");
}
