#![allow(clippy::disallowed_types)]

use serde_json::Value;

/// Validate `value` against `schema`, resolving `$ref` against `defs`
/// (`#/components/schemas/*` and `#/$defs/*`). Returns every violation found,
/// or `Ok(())`. Covers the OpenAPI-emitted subset: `type` (incl. unions and
/// `nullable`), `required`, `properties`, `additionalProperties`, `items`,
/// `enum`, `const`, and the `allOf`/`anyOf`/`oneOf` combinators. Unknown
/// keywords are ignored (lenient by construction), so undocumented extra
/// response fields never fail a call unless `additionalProperties: false` is
/// explicitly set.
pub fn validate(
    value: &Value,
    schema: &Value,
    defs: &Value,
) -> std::result::Result<(), Vec<String>> {
    let mut errs = Vec::new();
    check(value, schema, defs, "$", &mut errs, 0);
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

const MAX_DEPTH: usize = 128;

fn check(
    value: &Value,
    schema: &Value,
    defs: &Value,
    path: &str,
    errs: &mut Vec<String>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return; // recursion guard for cyclic $ref
    }
    // `true`/`{}` accept anything; `false` rejects everything.
    let obj = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errs.push(format!("{path}: schema `false` rejects all values"));
            return;
        }
        Value::Object(m) => m,
        _ => return,
    };

    // $ref indirection.
    if let Some(Value::String(r)) = obj.get("$ref") {
        match resolve_ref(r, defs) {
            Some(target) => check(value, target, defs, path, errs, depth + 1),
            None => errs.push(format!("{path}: unresolved $ref `{r}`")),
        }
        return;
    }

    // Combinators.
    if let Some(Value::Array(all)) = obj.get("allOf") {
        for sub in all {
            check(value, sub, defs, path, errs, depth + 1);
        }
    }
    if let Some(Value::Array(any)) = obj.get("anyOf") {
        let ok = any
            .iter()
            .any(|sub| validate_sub(value, sub, defs, depth + 1));
        if !ok {
            errs.push(format!("{path}: matched none of anyOf"));
        }
    }
    if let Some(Value::Array(one)) = obj.get("oneOf") {
        let matches = one
            .iter()
            .filter(|sub| validate_sub(value, sub, defs, depth + 1))
            .count();
        if matches != 1 {
            errs.push(format!(
                "{path}: matched {matches} of oneOf (want exactly 1)"
            ));
        }
    }

    // enum / const.
    if let Some(Value::Array(allowed)) = obj.get("enum")
        && !allowed.iter().any(|a| a == value)
    {
        errs.push(format!("{path}: value not in enum"));
    }
    if let Some(c) = obj.get("const")
        && c != value
    {
        errs.push(format!("{path}: value != const"));
    }

    // type (+ nullable).
    let nullable = obj
        .get("nullable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if value.is_null() && nullable {
        return;
    }
    if let Some(t) = obj.get("type")
        && !type_matches(value, t)
    {
        errs.push(format!("{path}: expected type {t}, got {}", kind_of(value)));
        return; // a type mismatch makes deeper checks noise
    }

    match value {
        Value::Object(vmap) => {
            if let Some(Value::Array(req)) = obj.get("required") {
                for r in req.iter().filter_map(Value::as_str) {
                    if !vmap.contains_key(r) {
                        errs.push(format!("{path}: missing required `{r}`"));
                    }
                }
            }
            let props = obj.get("properties").and_then(Value::as_object);
            if let Some(props) = props {
                for (k, sub) in props {
                    if let Some(child) = vmap.get(k) {
                        check(child, sub, defs, &format!("{path}.{k}"), errs, depth + 1);
                    }
                }
            }
            // additionalProperties: false rejects undeclared keys.
            if obj.get("additionalProperties") == Some(&Value::Bool(false))
                && let Some(props) = props
            {
                for k in vmap.keys() {
                    if !props.contains_key(k) {
                        errs.push(format!("{path}: unexpected property `{k}`"));
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = obj.get("items") {
                for (i, it) in items.iter().enumerate() {
                    check(
                        it,
                        item_schema,
                        defs,
                        &format!("{path}[{i}]"),
                        errs,
                        depth + 1,
                    );
                }
            }
        }
        _ => {}
    }
}

/// True if `value` validates against `schema` with no errors — used for the
/// anyOf/oneOf branch counting.
fn validate_sub(value: &Value, schema: &Value, defs: &Value, depth: usize) -> bool {
    let mut e = Vec::new();
    check(value, schema, defs, "$", &mut e, depth);
    e.is_empty()
}

/// Resolve a local `$ref` (`#/components/schemas/X`, `#/$defs/X`, or `#/X`)
/// against the shared `defs` pool. `defs` is expected to be the *contents* of
/// `components/schemas` (or `$defs`), so the leaf name is looked up directly;
/// a fully-qualified pointer falls back to walking `defs` by its tail segment.
fn resolve_ref<'a>(r: &str, defs: &'a Value) -> Option<&'a Value> {
    let leaf = r.rsplit('/').next()?;
    defs.get(leaf)
}

fn type_matches(value: &Value, t: &Value) -> bool {
    match t {
        Value::String(s) => single_type_matches(value, s),
        Value::Array(types) => types
            .iter()
            .filter_map(Value::as_str)
            .any(|s| single_type_matches(value, s)),
        _ => true,
    }
}

fn single_type_matches(value: &Value, t: &str) -> bool {
    match t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        // JSON has no integer type; accept any number with an integral value.
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "number" => value.is_number(),
        _ => true, // unknown type keyword: don't reject
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_defs() -> Value {
        json!({})
    }

    #[test]
    fn accepts_matching_object() {
        let schema = json!({
            "type": "object",
            "required": ["node"],
            "properties": { "node": { "type": "string" }, "full": { "type": "boolean" } }
        });
        let v = json!({ "node": "pve1", "full": true });
        assert!(validate(&v, &schema, &no_defs()).is_ok());
    }

    #[test]
    fn flags_missing_required_and_wrong_type() {
        let schema = json!({
            "type": "object",
            "required": ["node"],
            "properties": { "node": { "type": "string" } }
        });
        let v = json!({ "node": 5 });
        let errs = validate(&v, &schema, &no_defs()).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("node")), "{errs:?}");
    }

    #[test]
    fn integer_accepts_integral_number_rejects_fraction() {
        let s = json!({ "type": "integer" });
        assert!(validate(&json!(3), &s, &no_defs()).is_ok());
        assert!(validate(&json!(3.5), &s, &no_defs()).is_err());
    }

    #[test]
    fn nullable_allows_null() {
        let s = json!({ "type": "string", "nullable": true });
        assert!(validate(&Value::Null, &s, &no_defs()).is_ok());
    }

    #[test]
    fn resolves_ref_into_defs() {
        let defs = json!({ "Node": { "type": "object", "required": ["id"], "properties": { "id": { "type": "string" } } } });
        let schema = json!({ "$ref": "#/components/schemas/Node" });
        assert!(validate(&json!({ "id": "x" }), &schema, &defs).is_ok());
        assert!(validate(&json!({}), &schema, &defs).is_err());
    }

    #[test]
    fn additional_properties_false_rejects_extras() {
        let s = json!({ "type": "object", "properties": { "a": { "type": "string" } }, "additionalProperties": false });
        assert!(validate(&json!({ "a": "x" }), &s, &no_defs()).is_ok());
        assert!(validate(&json!({ "a": "x", "b": 1 }), &s, &no_defs()).is_err());
    }

    #[test]
    fn lenient_on_unknown_extra_fields_by_default() {
        // No additionalProperties → extra response fields are accepted (real
        // appliances return undocumented keys).
        let s = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        assert!(validate(&json!({ "a": "x", "extra": 99 }), &s, &no_defs()).is_ok());
    }

    #[test]
    fn any_of_matches_one_branch() {
        let s = json!({ "anyOf": [ { "type": "string" }, { "type": "integer" } ] });
        assert!(validate(&json!("x"), &s, &no_defs()).is_ok());
        assert!(validate(&json!(3), &s, &no_defs()).is_ok());
        assert!(validate(&json!(true), &s, &no_defs()).is_err());
    }

    #[test]
    fn single_type_matches_all_keywords() {
        assert!(single_type_matches(&json!({}), "object"));
        assert!(single_type_matches(&json!([]), "array"));
        assert!(single_type_matches(&json!("s"), "string"));
        assert!(single_type_matches(&json!(true), "boolean"));
        assert!(single_type_matches(&Value::Null, "null"));
        assert!(single_type_matches(&json!(4), "integer"));
        assert!(single_type_matches(&json!(4.0), "integer"));
        assert!(!single_type_matches(&json!(4.5), "integer"));
        assert!(single_type_matches(&json!(4.5), "number"));
        assert!(single_type_matches(&json!("x"), "unknownkw"));
    }

    #[test]
    fn kind_of_names_every_variant() {
        assert_eq!(kind_of(&Value::Null), "null");
        assert_eq!(kind_of(&json!(true)), "boolean");
        assert_eq!(kind_of(&json!(1)), "number");
        assert_eq!(kind_of(&json!("s")), "string");
        assert_eq!(kind_of(&json!([])), "array");
        assert_eq!(kind_of(&json!({})), "object");
    }

    #[test]
    fn type_union_array_matches_any() {
        let s = json!({ "type": ["string", "null"] });
        assert!(validate(&json!("x"), &s, &no_defs()).is_ok());
        assert!(validate(&Value::Null, &s, &no_defs()).is_ok());
        assert!(validate(&json!(3), &s, &no_defs()).is_err());
    }

    #[test]
    fn bool_schema_true_and_false() {
        assert!(validate(&json!(1), &json!(true), &no_defs()).is_ok());
        assert!(validate(&json!(1), &json!(false), &no_defs()).is_err());
    }

    #[test]
    fn all_of_requires_every_subschema() {
        let s = json!({
            "allOf": [
                { "type": "object", "required": ["a"] },
                { "type": "object", "required": ["b"] }
            ]
        });
        assert!(validate(&json!({ "a": 1, "b": 2 }), &s, &no_defs()).is_ok());
        assert!(validate(&json!({ "a": 1 }), &s, &no_defs()).is_err());
    }

    #[test]
    fn one_of_wants_exactly_one() {
        let s = json!({ "oneOf": [ { "type": "string" }, { "type": "integer" } ] });
        assert!(validate(&json!("x"), &s, &no_defs()).is_ok());
        // A plain integer matches "integer" only.
        assert!(validate(&json!(3), &s, &no_defs()).is_ok());
        // Matches neither ⇒ error.
        assert!(validate(&json!(true), &s, &no_defs()).is_err());
    }

    #[test]
    fn const_must_equal() {
        let s = json!({ "const": "fixed" });
        assert!(validate(&json!("fixed"), &s, &no_defs()).is_ok());
        assert!(validate(&json!("other"), &s, &no_defs()).is_err());
    }

    #[test]
    fn enum_membership() {
        let s = json!({ "enum": ["a", "b"] });
        assert!(validate(&json!("a"), &s, &no_defs()).is_ok());
        assert!(validate(&json!("z"), &s, &no_defs()).is_err());
    }

    #[test]
    fn unresolved_ref_reports_error() {
        let s = json!({ "$ref": "#/components/schemas/Missing" });
        let errs = validate(&json!({}), &s, &no_defs()).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unresolved $ref")),
            "{errs:?}"
        );
    }

    #[test]
    fn array_items_validated_elementwise() {
        let s = json!({ "type": "array", "items": { "type": "integer" } });
        assert!(validate(&json!([1, 2, 3]), &s, &no_defs()).is_ok());
        let errs = validate(&json!([1, "bad"]), &s, &no_defs()).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("[1]")), "{errs:?}");
    }

    #[test]
    fn non_object_non_bool_schema_accepts() {
        // A schema that isn't an object/bool (e.g. a bare string) accepts anything.
        assert!(validate(&json!(1), &json!("weird"), &no_defs()).is_ok());
    }

    #[test]
    fn type_keyword_non_string_non_array_accepts_any() {
        // A malformed `type` (neither string nor array) hits the permissive arm
        // of type_matches and never rejects.
        let s = json!({ "type": 5 });
        assert!(validate(&json!("anything"), &s, &no_defs()).is_ok());
        assert!(validate(&json!(42), &s, &no_defs()).is_ok());
    }

    #[test]
    fn resolve_ref_by_qualified_tail_segment() {
        // A fully-qualified pointer resolves by its leaf name against the pool.
        let defs = json!({ "Widget": { "type": "string" } });
        let s = json!({ "$ref": "#/$defs/Widget" });
        assert!(validate(&json!("ok"), &s, &defs).is_ok());
        assert!(validate(&json!(3), &s, &defs).is_err());
    }
}
