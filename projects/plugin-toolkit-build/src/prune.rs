//! Reachability-based OpenAPI pruning for large upstream specs.
//!
//! Some upstream specs are enormous — Jellyfin's is ~300 paths and several
//! hundred `components/schemas`. progenitor generates a Rust type for *every*
//! schema in `components/schemas`, regardless of which paths reference it, so
//! feeding it the full document produces a vast generated module the plugin
//! does not use.
//!
//! This pass trims an OpenAPI document down to a caller-supplied keep-list of
//! paths, then computes the transitive `$ref` closure over the reusable
//! component sections (`schemas`, `parameters`, `responses`, `headers`, …)
//! starting from those paths and deletes every component not reachable. The
//! result is a self-consistent, much smaller document that codegens only the
//! surface the plugin actually drives.
//!
//! Like the 3.1 lowering pass, this operates on the raw JSON `Value`: the job
//! is a transformation over an open-ended upstream tree orca does not own, so
//! there is no fixed struct to deserialize into. Only `paths` and the
//! ref-counted `components/<section>` objects are touched; sections never
//! reached by a `$ref` (`securitySchemes`, `tags`, `info`, …) pass through
//! untouched.
//!
//! Every `#/components/<section>/<name>` `$ref` is followed generically — a
//! kept path that references a shared `parameters`/`responses`/`headers`
//! component pulls that component in, and the component's own refs (e.g. a
//! parameter's `schema`) seed further reachability. Only `$ref`s that aren't
//! under `#/components/` at all are ignored (external/document-local refs the
//! supported specs don't use).

// Operates on the raw upstream OpenAPI tree, which orca does not own and which
// has no fixed struct to deserialize into (it may be 3.1 pre-lowering, and the
// whole job is rewriting an open-ended JSON document). Same stance as
// `openapi::lower_31`. See module docs.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde_json::value::Value;

const COMPONENTS_PREFIX: &str = "#/components/";

/// A reachable component, identified by its section and name
/// (e.g. `("schemas", "MediaContainer")` or `("parameters", "accepts")`).
type CompRef = (String, String);

/// Prune `spec` to only `keep_paths` and the components they transitively
/// reference. `keep_paths` are exact path keys (e.g. `"/System/Info"`).
///
/// Returns the sorted list of retained `schemas` names, for build-log
/// visibility (the schemas are what progenitor turns into Rust types).
pub fn to_paths(spec: &mut Value, keep_paths: &[&str]) -> Result<Vec<String>> {
    let keep: BTreeSet<&str> = keep_paths.iter().copied().collect();

    // --- 1. trim paths ---
    let Some(paths) = spec.get_mut("paths").and_then(Value::as_object_mut) else {
        bail!("openapi prune: document has no `paths` object");
    };
    let present: BTreeSet<String> = paths.keys().cloned().collect();
    for want in &keep {
        if !present.contains(*want) {
            bail!("openapi prune: keep-path {want:?} not present in spec");
        }
    }
    paths.retain(|k, _| keep.contains(k.as_str()));

    // --- 2. seed roots: every component $ref reachable from the kept paths ---
    let paths_val = spec.get("paths").cloned().unwrap_or(Value::Null);
    let mut reachable: BTreeSet<CompRef> = BTreeSet::new();
    let mut frontier: Vec<CompRef> = Vec::new();
    collect_refs(&paths_val, &mut frontier)?;

    // --- 3. transitive closure over every referenced component section ---
    let components_owned = spec
        .get("components")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    while let Some(comp) = frontier.pop() {
        if !reachable.insert(comp.clone()) {
            continue;
        }
        let (section, name) = &comp;
        if let Some(def) = components_owned.get(section).and_then(|s| s.get(name)) {
            let mut child = Vec::new();
            collect_refs(def, &mut child)?;
            frontier.extend(child);
        }
    }

    // --- 4. delete unreachable members of every referenced section ---
    // Only sections that appear in a $ref are pruned; sections never reached
    // (securitySchemes, …) pass through untouched.
    let referenced_sections: BTreeSet<&str> = reachable
        .iter()
        .map(|(section, _)| section.as_str())
        .collect();
    if let Some(components) = spec.get_mut("components").and_then(Value::as_object_mut) {
        for section in &referenced_sections {
            if let Some(members) = components.get_mut(*section).and_then(Value::as_object_mut) {
                members
                    .retain(|name, _| reachable.contains(&((*section).to_string(), name.clone())));
            }
        }
    }

    Ok(reachable
        .into_iter()
        .filter(|(section, _)| section == "schemas")
        .map(|(_, name)| name)
        .collect())
}

/// Walk `value` and push every `#/components/<section>/<name>` `$ref` into
/// `out` as a `(section, name)` pair. `$ref`s not under `#/components/`
/// (external or document-local) are ignored — the supported specs don't use
/// them.
fn collect_refs(value: &Value, out: &mut Vec<CompRef>) -> Result<()> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref")
                && let Some(rest) = r.strip_prefix(COMPONENTS_PREFIX)
            {
                // `<section>/<name>` — section is the first segment, name the
                // remainder (component names are flat, so this is one segment).
                if let Some((section, name)) = rest.split_once('/') {
                    out.push((section.to_string(), name.to_string()));
                } else {
                    bail!("openapi prune: malformed component $ref {r:?}");
                }
            }
            for v in map.values() {
                collect_refs(v, out)?;
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_refs(v, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "openapi": "3.0.3",
            "info": { "title": "t", "version": "0" },
            "paths": {
                "/keep": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": { "$ref": "#/components/schemas/Used" }
                                    }
                                }
                            }
                        }
                    }
                },
                "/drop": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": { "$ref": "#/components/schemas/Unused" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "components": {
                "schemas": {
                    "Used": {
                        "type": "object",
                        "properties": { "child": { "$ref": "#/components/schemas/Child" } }
                    },
                    "Child": { "type": "string" },
                    "Unused": { "type": "object" },
                    "AlsoUnused": { "$ref": "#/components/schemas/Unused" }
                }
            }
        })
    }

    #[test]
    fn keeps_only_selected_paths() {
        let mut v = doc();
        to_paths(&mut v, &["/keep"]).unwrap();
        let paths = v["paths"].as_object().unwrap();
        assert!(paths.contains_key("/keep"));
        assert!(!paths.contains_key("/drop"));
    }

    #[test]
    fn keeps_transitively_referenced_schemas_only() {
        let mut v = doc();
        let kept = to_paths(&mut v, &["/keep"]).unwrap();
        assert_eq!(kept, vec!["Child".to_string(), "Used".to_string()]);
        let schemas = v["components"]["schemas"].as_object().unwrap();
        assert!(schemas.contains_key("Used"));
        assert!(schemas.contains_key("Child"));
        assert!(!schemas.contains_key("Unused"));
        assert!(!schemas.contains_key("AlsoUnused"));
    }

    #[test]
    fn errors_on_missing_keep_path() {
        let mut v = doc();
        let err = to_paths(&mut v, &["/nope"]).unwrap_err().to_string();
        assert!(err.contains("not present"), "got: {err}");
    }

    #[test]
    fn follows_parameter_refs_and_their_schemas() {
        // A kept path references a shared parameter, which itself references a
        // schema. Both must be retained; an unreferenced parameter is dropped.
        let mut v = json!({
            "paths": {
                "/x": {
                    "get": {
                        "parameters": [ { "$ref": "#/components/parameters/Used" } ],
                        "responses": { "200": { "description": "ok" } }
                    }
                }
            },
            "components": {
                "parameters": {
                    "Used": {
                        "name": "u", "in": "query",
                        "schema": { "$ref": "#/components/schemas/PSchema" }
                    },
                    "Unused": { "name": "z", "in": "query" }
                },
                "schemas": {
                    "PSchema": { "type": "string" },
                    "OtherUnused": { "type": "object" }
                }
            }
        });
        let kept_schemas = to_paths(&mut v, &["/x"]).unwrap();
        assert_eq!(kept_schemas, vec!["PSchema".to_string()]);
        let params = v["components"]["parameters"].as_object().unwrap();
        assert!(params.contains_key("Used"));
        assert!(!params.contains_key("Unused"));
        let schemas = v["components"]["schemas"].as_object().unwrap();
        assert!(schemas.contains_key("PSchema"));
        assert!(!schemas.contains_key("OtherUnused"));
    }

    #[test]
    fn leaves_unreferenced_sections_untouched() {
        // securitySchemes is never reached by a $ref — it must survive pruning.
        let mut v = json!({
            "paths": { "/x": { "get": { "responses": { "200": { "description": "ok" } } } } },
            "components": {
                "schemas": { "Unused": { "type": "object" } },
                "securitySchemes": { "apiKey": { "type": "apiKey", "name": "k", "in": "header" } }
            }
        });
        to_paths(&mut v, &["/x"]).unwrap();
        assert!(
            v["components"]["securitySchemes"]
                .as_object()
                .unwrap()
                .contains_key("apiKey")
        );
    }
}
