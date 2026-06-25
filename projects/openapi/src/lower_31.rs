//! Lower an OpenAPI **3.1** document to a **3.0** one so that `progenitor`
//! (which only understands the 3.0 type system, via `openapiv3` 2.x) can
//! consume it.
//!
//! `openapiv3` 2.x models OpenAPI 3.0 and will fail or mis-parse a `3.1`
//! document directly — `type: ["string","null"]` arrays and numeric
//! `exclusiveMinimum` have no 3.0 deserialization. So this pass operates on
//! the raw JSON `Value` (parsed from JSON *or* YAML), rewrites the 3.1-only
//! constructs into their 3.0 equivalents, sets `openapi` to `3.0.3`, and
//! hands the lowered value back. The caller then deserializes it into
//! `openapiv3::OpenAPI` and runs the existing
//! [`crate::normalize::for_progenitor`] + progenitor.
//!
//! ```ignore
//! let mut value: serde_json::value::Value = serde_yaml::from_str(&raw)?;
//! if openapi::lower_31::is_31(&value) {
//!     let report = openapi::lower_31::lower_to_30(&mut value)?;
//!     report.emit_cargo_warnings("plex");
//! }
//! let mut spec: openapiv3::OpenAPI = serde_json::value::from_value(value)?;
//! openapi::normalize::for_progenitor(&mut spec);
//! ```
//!
//! `Value` is the right model here: this is a *transformation over arbitrary
//! upstream OpenAPI documents* — the whole job is rewriting an open-ended
//! JSON tree that orca does not own. There is no fixed struct to deserialize
//! into (the input is, by construction, not yet a valid `openapiv3` doc), so
//! the workspace "model it as a typed struct" guidance does not apply. Schema
//! keywords are addressed by name; everything else is passed through opaque.
//!
//! Be NARROW: only the constructs listed below are transformed; everything
//! else passes through untouched. 3.1 features with no faithful 3.0 lowering
//! (`prefixItems`, `if`/`then`/`else`, `unevaluatedProperties`,
//! `unevaluatedItems`, top-level `webhooks`) error LOUDLY rather than being
//! silently dropped — failing is the spec.

use anyhow::{Result, bail};
use serde_json::value::Value;

/// What [`lower_to_30`] had to change. Surfaced so consumer build scripts can
/// `cargo:warning=` each entry — mirrors [`crate::normalize::NormalizeReport`]
/// so a future upstream spec growing a new 3.1 construct shows up in the build
/// log instead of disappearing from the generated client.
#[derive(Debug, Default, Clone)]
pub struct Lowering {
    /// `type: [..,"null"]` arrays collapsed to a scalar `type` + `nullable`.
    /// Each entry is the JSON pointer of the schema object.
    pub nullable_type_arrays: Vec<String>,
    /// Schema-level `examples` arrays (3.1) replaced by a singular `example`
    /// (3.0). `(pointer, dropped_count)` — `dropped_count` is how many array
    /// elements past the first were discarded.
    pub examples_to_example: Vec<(String, usize)>,
    /// Numeric `exclusiveMinimum`/`exclusiveMaximum` (3.1) rewritten to the
    /// 3.0 `minimum`/`maximum` + boolean form. `(pointer, keyword)`.
    pub numeric_exclusive: Vec<(String, String)>,
    /// `const` keywords rewritten to a single-value `enum`. Pointer of the
    /// schema object.
    pub const_to_enum: Vec<String>,
    /// `$schema` keywords dropped (forbidden in 3.0). Pointer of the schema.
    pub dropped_schema_keyword: Vec<String>,
    /// `contentMediaType`/`contentEncoding` keywords dropped (progenitor
    /// ignores them). `(pointer, keyword)`.
    pub dropped_content_keywords: Vec<(String, String)>,
}

impl Lowering {
    /// Emit `cargo:warning=` lines so each lowering appears in the build log.
    /// Intended for use from a consumer's build.rs.
    pub fn emit_cargo_warnings(&self, tag: &str) {
        for ptr in &self.nullable_type_arrays {
            println!(
                "cargo:warning={tag}: lowered nullable type array at {ptr} -> type + nullable"
            );
        }
        for (ptr, dropped) in &self.examples_to_example {
            println!(
                "cargo:warning={tag}: lowered schema examples[] at {ptr} -> example (dropped {dropped} extra value(s))"
            );
        }
        for (ptr, kw) in &self.numeric_exclusive {
            println!(
                "cargo:warning={tag}: lowered numeric {kw} at {ptr} -> minimum/maximum + boolean form"
            );
        }
        for ptr in &self.const_to_enum {
            println!("cargo:warning={tag}: lowered const at {ptr} -> single-value enum");
        }
        for ptr in &self.dropped_schema_keyword {
            println!("cargo:warning={tag}: dropped $schema keyword at {ptr} (forbidden in 3.0)");
        }
        for (ptr, kw) in &self.dropped_content_keywords {
            println!("cargo:warning={tag}: dropped {kw} at {ptr} (progenitor ignores it)");
        }
    }
}

/// True when the document's top-level `openapi` field begins with `3.1`.
pub fn is_31(spec_json: &Value) -> bool {
    spec_json
        .get("openapi")
        .and_then(Value::as_str)
        .is_some_and(|v| v.starts_with("3.1"))
}

/// Lower a 3.1 document to 3.0 in place. Returns a report of every transform
/// applied. Errors loudly (naming the JSON pointer) on 3.1 constructs that
/// have no faithful 3.0 lowering, rather than silently dropping them.
pub fn lower_to_30(spec_json: &mut Value) -> Result<Lowering> {
    let mut report = Lowering::default();

    if spec_json.get("webhooks").is_some() {
        bail!(
            "openapi: cannot lower 3.1 -> 3.0: top-level `webhooks` (at /webhooks) has no 3.0 equivalent"
        );
    }

    walk(spec_json, "", &mut report)?;

    if let Some(obj) = spec_json.as_object_mut() {
        obj.insert("openapi".into(), Value::String("3.0.3".into()));
    }

    Ok(report)
}

/// Recurse through the entire document. Every object encountered is treated
/// as a *potential* schema and run through [`lower_schema_object`]; that fn
/// only touches schema-specific keywords, so non-schema objects (info, paths,
/// responses, …) pass through untouched. This single recursive walk reaches
/// `components/schemas`, inline parameter/requestBody/response schemas,
/// `items`, `properties`, `additionalProperties`, and the
/// `allOf`/`anyOf`/`oneOf`/`not` arrays without enumerating each site.
fn walk(value: &mut Value, pointer: &str, report: &mut Lowering) -> Result<()> {
    match value {
        Value::Object(_) => {
            lower_schema_object(value, pointer, report)?;
            // `lower_schema_object` may have reshaped the map; re-borrow.
            let Value::Object(map) = value else {
                return Ok(());
            };
            for (k, v) in map.iter_mut() {
                let child = format!("{pointer}/{}", escape_token(k));
                walk(v, &child, report)?;
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter_mut().enumerate() {
                let child = format!("{pointer}/{i}");
                walk(v, &child, report)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Apply the schema-level lowering rules to a single object node. Only
/// schema-specific keywords are touched; the same object's non-schema
/// children are handled by [`walk`]. Errors on unsupported 3.1 constructs.
fn lower_schema_object(value: &mut Value, pointer: &str, report: &mut Lowering) -> Result<()> {
    let Some(map) = value.as_object_mut() else {
        return Ok(());
    };

    // --- error constructs: no faithful 3.0 lowering exists ---
    for kw in [
        "prefixItems",
        "if",
        "then",
        "else",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if map.contains_key(kw) {
            bail!(
                "openapi: cannot lower 3.1 -> 3.0: `{kw}` at {pointer} has no 3.0 equivalent (failing loudly rather than dropping it)"
            );
        }
    }

    // --- $schema: forbidden in 3.0, drop it ---
    if map.remove("$schema").is_some() {
        report.dropped_schema_keyword.push(ptr_or_root(pointer));
    }

    // --- contentMediaType / contentEncoding: progenitor ignores, drop ---
    for kw in ["contentMediaType", "contentEncoding"] {
        if map.remove(kw).is_some() {
            report
                .dropped_content_keywords
                .push((ptr_or_root(pointer), kw.to_string()));
        }
    }

    // --- nullable type array: ["string","null"] -> "string" + nullable ---
    if let Some(Value::Array(types)) = map.get("type") {
        let has_null = types.iter().any(|t| t.as_str() == Some("null"));
        let non_null: Vec<&Value> = types
            .iter()
            .filter(|t| t.as_str() != Some("null"))
            .collect();
        if has_null && non_null.len() == 1 {
            let kept = non_null[0].clone();
            map.insert("type".into(), kept);
            map.insert("nullable".into(), Value::Bool(true));
            report.nullable_type_arrays.push(ptr_or_root(pointer));
        } else if has_null {
            bail!(
                "openapi: cannot lower 3.1 -> 3.0: multi-type union `type` array at {pointer} (3.0 cannot express a union of {} non-null types)",
                non_null.len()
            );
        } else if types.len() > 1 {
            bail!(
                "openapi: cannot lower 3.1 -> 3.0: multi-type `type` array at {pointer} (3.0 allows only a single type)"
            );
        } else if types.len() == 1 {
            // A single-element array without "null" is unusual but harmless;
            // collapse it so openapiv3 accepts it.
            let kept = types[0].clone();
            map.insert("type".into(), kept);
        }
    }

    // --- numeric exclusiveMinimum/Maximum -> minimum/maximum + boolean ---
    for (excl, bound) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        if let Some(v) = map.get(excl)
            && v.is_number()
        {
            let num = v.clone();
            map.insert(bound.into(), num);
            map.insert(excl.into(), Value::Bool(true));
            report
                .numeric_exclusive
                .push((ptr_or_root(pointer), excl.to_string()));
        }
    }

    // --- const X -> enum [X] ---
    if let Some(c) = map.remove("const") {
        map.insert("enum".into(), Value::Array(vec![c]));
        report.const_to_enum.push(ptr_or_root(pointer));
    }

    // --- schema-level examples ARRAY (3.1) -> example (3.0) ---
    // The map form (`examples: { name: {...} }`) on media-type/parameter
    // objects is valid 3.0 — leave it. Only the array form is 3.1-only.
    if let Some(Value::Array(arr)) = map.get("examples") {
        if let Some(first) = arr.first().cloned() {
            let dropped = arr.len().saturating_sub(1);
            map.remove("examples");
            map.insert("example".into(), first);
            report
                .examples_to_example
                .push((ptr_or_root(pointer), dropped));
        } else {
            // Empty examples array — just drop it.
            map.remove("examples");
            report.examples_to_example.push((ptr_or_root(pointer), 0));
        }
    }

    Ok(())
}

fn ptr_or_root(pointer: &str) -> String {
    if pointer.is_empty() {
        "/".to_string()
    } else {
        pointer.to_string()
    }
}

/// RFC 6901 token escaping for JSON pointers (`~` -> `~0`, `/` -> `~1`).
fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(schemas: Value) -> Value {
        json!({
            "openapi": "3.1.0",
            "info": { "title": "t", "version": "0" },
            "paths": {},
            "components": { "schemas": schemas }
        })
    }

    #[test]
    fn is_31_detects_version() {
        assert!(is_31(&json!({ "openapi": "3.1.0" })));
        assert!(is_31(&json!({ "openapi": "3.1.1" })));
        assert!(!is_31(&json!({ "openapi": "3.0.3" })));
        assert!(!is_31(&json!({ "openapi": "3.0.0" })));
        assert!(!is_31(&json!({})));
    }

    #[test]
    fn lower_sets_openapi_version() {
        let mut v = base(json!({}));
        lower_to_30(&mut v).unwrap();
        assert_eq!(v["openapi"], json!("3.0.3"));
    }

    #[test]
    fn nullable_type_array_two_types() {
        let mut v = base(json!({
            "A": { "type": ["string", "null"] }
        }));
        let r = lower_to_30(&mut v).unwrap();
        assert_eq!(v["components"]["schemas"]["A"]["type"], json!("string"));
        assert_eq!(v["components"]["schemas"]["A"]["nullable"], json!(true));
        assert_eq!(r.nullable_type_arrays.len(), 1);
        assert!(r.nullable_type_arrays[0].ends_with("/components/schemas/A"));
    }

    #[test]
    fn nullable_type_array_null_first() {
        let mut v = base(json!({ "A": { "type": ["null", "integer"] } }));
        lower_to_30(&mut v).unwrap();
        assert_eq!(v["components"]["schemas"]["A"]["type"], json!("integer"));
        assert_eq!(v["components"]["schemas"]["A"]["nullable"], json!(true));
    }

    #[test]
    fn nullable_type_array_error_on_three_types() {
        let mut v = base(json!({
            "A": { "type": ["string", "integer", "null"] }
        }));
        let err = lower_to_30(&mut v).unwrap_err().to_string();
        assert!(err.contains("multi-type union"), "got: {err}");
        assert!(err.contains("/components/schemas/A"), "got: {err}");
    }

    #[test]
    fn type_array_error_on_multiple_without_null() {
        let mut v = base(json!({
            "A": { "type": ["string", "integer"] }
        }));
        let err = lower_to_30(&mut v).unwrap_err().to_string();
        assert!(err.contains("multi-type `type` array"), "got: {err}");
    }

    #[test]
    fn single_element_type_array_collapses() {
        let mut v = base(json!({ "A": { "type": ["string"] } }));
        lower_to_30(&mut v).unwrap();
        assert_eq!(v["components"]["schemas"]["A"]["type"], json!("string"));
    }

    #[test]
    fn examples_array_to_example() {
        let mut v = base(json!({
            "A": { "type": "string", "examples": ["a", "b", "c"] }
        }));
        let r = lower_to_30(&mut v).unwrap();
        assert_eq!(v["components"]["schemas"]["A"]["example"], json!("a"));
        assert!(v["components"]["schemas"]["A"].get("examples").is_none());
        assert_eq!(r.examples_to_example.len(), 1);
        assert_eq!(r.examples_to_example[0].1, 2);
    }

    #[test]
    fn examples_map_left_alone() {
        // The map form (keyed examples on a media type) is valid 3.0 and
        // must NOT be touched.
        let mut v = json!({
            "openapi": "3.1.0",
            "info": { "title": "t", "version": "0" },
            "paths": {
                "/a": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": { "type": "string" },
                                        "examples": {
                                            "sample": { "value": "x" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let r = lower_to_30(&mut v).unwrap();
        let mt = &v["paths"]["/a"]["get"]["responses"]["200"]["content"]["application/json"];
        assert!(mt.get("examples").is_some(), "examples map should remain");
        assert!(mt.get("example").is_none());
        assert!(r.examples_to_example.is_empty());
    }

    #[test]
    fn numeric_exclusive_minimum_to_minimum_plus_bool() {
        let mut v = base(json!({
            "A": { "type": "integer", "exclusiveMinimum": 0, "exclusiveMaximum": 10 }
        }));
        let r = lower_to_30(&mut v).unwrap();
        let a = &v["components"]["schemas"]["A"];
        assert_eq!(a["minimum"], json!(0));
        assert_eq!(a["exclusiveMinimum"], json!(true));
        assert_eq!(a["maximum"], json!(10));
        assert_eq!(a["exclusiveMaximum"], json!(true));
        assert_eq!(r.numeric_exclusive.len(), 2);
    }

    #[test]
    fn boolean_exclusive_minimum_left_alone() {
        let mut v = base(json!({
            "A": { "type": "integer", "minimum": 0, "exclusiveMinimum": true }
        }));
        let r = lower_to_30(&mut v).unwrap();
        let a = &v["components"]["schemas"]["A"];
        assert_eq!(a["exclusiveMinimum"], json!(true));
        assert_eq!(a["minimum"], json!(0));
        assert!(r.numeric_exclusive.is_empty());
    }

    #[test]
    fn const_to_enum() {
        let mut v = base(json!({
            "A": { "const": "fixed" }
        }));
        let r = lower_to_30(&mut v).unwrap();
        assert_eq!(v["components"]["schemas"]["A"]["enum"], json!(["fixed"]));
        assert!(v["components"]["schemas"]["A"].get("const").is_none());
        assert_eq!(r.const_to_enum.len(), 1);
    }

    #[test]
    fn schema_keyword_dropped() {
        let mut v = base(json!({
            "A": { "$schema": "https://json-schema.org/draft/2020-12/schema", "type": "string" }
        }));
        let r = lower_to_30(&mut v).unwrap();
        assert!(v["components"]["schemas"]["A"].get("$schema").is_none());
        assert_eq!(r.dropped_schema_keyword.len(), 1);
    }

    #[test]
    fn content_keywords_dropped() {
        let mut v = base(json!({
            "A": { "type": "string", "contentMediaType": "image/png", "contentEncoding": "base64" }
        }));
        let r = lower_to_30(&mut v).unwrap();
        let a = &v["components"]["schemas"]["A"];
        assert!(a.get("contentMediaType").is_none());
        assert!(a.get("contentEncoding").is_none());
        assert_eq!(r.dropped_content_keywords.len(), 2);
    }

    #[test]
    fn prefix_items_errors() {
        let mut v = base(json!({
            "A": { "type": "array", "prefixItems": [{ "type": "string" }] }
        }));
        let err = lower_to_30(&mut v).unwrap_err().to_string();
        assert!(err.contains("prefixItems"), "got: {err}");
        assert!(err.contains("/components/schemas/A"), "got: {err}");
    }

    #[test]
    fn if_then_else_errors() {
        let mut v = base(json!({
            "A": { "if": { "type": "string" }, "then": { "minLength": 1 } }
        }));
        let err = lower_to_30(&mut v).unwrap_err().to_string();
        assert!(err.contains("at"), "got: {err}");
        assert!(err.contains("if") || err.contains("then"), "got: {err}");
    }

    #[test]
    fn unevaluated_properties_errors() {
        let mut v = base(json!({
            "A": { "type": "object", "unevaluatedProperties": false }
        }));
        let err = lower_to_30(&mut v).unwrap_err().to_string();
        assert!(err.contains("unevaluatedProperties"), "got: {err}");
    }

    #[test]
    fn webhooks_errors() {
        let mut v = json!({
            "openapi": "3.1.0",
            "info": { "title": "t", "version": "0" },
            "paths": {},
            "webhooks": { "newThing": { "post": {} } }
        });
        let err = lower_to_30(&mut v).unwrap_err().to_string();
        assert!(err.contains("webhooks"), "got: {err}");
    }

    #[test]
    fn lowers_nested_schemas_recursively() {
        // Exercise items, properties, additionalProperties, allOf/anyOf.
        let mut v = base(json!({
            "A": {
                "type": "object",
                "properties": {
                    "list": {
                        "type": "array",
                        "items": { "type": ["string", "null"] }
                    },
                    "extra": { "const": 42 }
                },
                "additionalProperties": { "exclusiveMinimum": 1 },
                "allOf": [
                    { "type": ["integer", "null"] }
                ]
            }
        }));
        let r = lower_to_30(&mut v).unwrap();
        let a = &v["components"]["schemas"]["A"];
        assert_eq!(a["properties"]["list"]["items"]["type"], json!("string"));
        assert_eq!(a["properties"]["list"]["items"]["nullable"], json!(true));
        assert_eq!(a["properties"]["extra"]["enum"], json!([42]));
        assert_eq!(a["additionalProperties"]["minimum"], json!(1));
        assert_eq!(a["additionalProperties"]["exclusiveMinimum"], json!(true));
        assert_eq!(a["allOf"][0]["type"], json!("integer"));
        assert_eq!(a["allOf"][0]["nullable"], json!(true));
        assert_eq!(r.nullable_type_arrays.len(), 2);
        assert_eq!(r.const_to_enum.len(), 1);
        assert_eq!(r.numeric_exclusive.len(), 1);
    }

    #[test]
    fn idempotent_after_lowering() {
        // After one lowering, the result is 3.0-shaped; running again finds
        // nothing to change (and the version gate would skip it in practice).
        let mut v = base(json!({
            "A": { "type": ["string", "null"], "examples": ["x"] }
        }));
        lower_to_30(&mut v).unwrap();
        let snapshot = v.clone();
        let r2 = lower_to_30(&mut v).unwrap();
        assert_eq!(v, snapshot, "second pass must be a no-op");
        assert!(r2.nullable_type_arrays.is_empty());
        assert!(r2.examples_to_example.is_empty());
        assert!(r2.const_to_enum.is_empty());
    }

    #[test]
    fn lowered_spec_deserializes_into_openapiv3() {
        let mut v = base(json!({
            "Thing": {
                "type": "object",
                "properties": {
                    "name": { "type": ["string", "null"] },
                    "count": { "type": "integer", "exclusiveMinimum": 0 },
                    "kind": { "const": "widget" }
                }
            }
        }));
        lower_to_30(&mut v).unwrap();
        let spec: openapiv3::OpenAPI =
            serde_json::value::from_value(v).expect("lowered spec must deserialize into openapiv3");
        assert_eq!(spec.openapi, "3.0.3");
    }

    #[test]
    fn emit_cargo_warnings_covers_all_buckets() {
        let r = Lowering {
            nullable_type_arrays: vec!["/components/schemas/A".into()],
            examples_to_example: vec![("/components/schemas/A".into(), 2)],
            numeric_exclusive: vec![("/components/schemas/A".into(), "exclusiveMinimum".into())],
            const_to_enum: vec!["/components/schemas/A".into()],
            dropped_schema_keyword: vec!["/components/schemas/A".into()],
            dropped_content_keywords: vec![(
                "/components/schemas/A".into(),
                "contentEncoding".into(),
            )],
        };
        r.emit_cargo_warnings("test");
    }
}
