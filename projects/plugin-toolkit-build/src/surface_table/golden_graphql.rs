use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ::openapi::surface::ApiSurface;

use super::golden::existing_tools;
use super::graphql::{Options, generate};

const OVERRIDE_FIELDS: &[&str] = &["from", "api_key", "insecure"];

fn diff(schemas: &Path, queries: &Path, name: &str) -> (Vec<String>, BTreeSet<String>) {
    let tmp =
        std::env::temp_dir().join(format!("surface-golden-gql-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let modules = crate::graphql::render_modules(schemas, queries).unwrap();
    std::fs::write(tmp.join("modules.rs"), modules).unwrap();
    crate::surface::graphql::generate(&tmp, queries, name).unwrap();
    let src = std::fs::read_to_string(tmp.join(format!("{name}_surface.rs"))).unwrap();
    let path = generate(schemas, queries, &tmp, name, &Options::default()).unwrap();
    let table = ApiSurface::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    std::fs::remove_dir_all(&tmp).unwrap();

    let existing = existing_tools(&src, name);
    let mut problems = Vec::new();
    let public: BTreeSet<&str> = table
        .ops
        .iter()
        .filter(|op| !op.internal)
        .map(|op| op.tool.as_str())
        .collect();
    for op in &table.ops {
        let mutation = table.is_write(op);
        if mutation && (op.policy.role != "admin" || !op.policy.execute_gated) {
            problems.push(format!(
                "{}: mutation is not admin+gated: {:?}",
                op.tool, op.policy
            ));
        }
        if !existing.contains_key(&op.tool) && !op.internal {
            let read_shaped = verb_shape::is_read_shaped(op.tool.rsplit('.').next().unwrap());
            let role = if !mutation && read_shaped {
                "read"
            } else {
                "admin"
            };
            if op.policy.role != role || op.policy.execute_gated != (mutation || !read_shaped) {
                problems.push(format!("{}: new tool policy {:?}", op.tool, op.policy));
            }
        }
    }
    let old_names: BTreeSet<&str> = existing.keys().map(String::as_str).collect();
    if let Some(missing) = old_names.difference(&public).next() {
        problems.push(format!("{missing}: missing from table"));
    }
    let extra = public
        .difference(&old_names)
        .map(|s| s.to_string())
        .collect();
    for (tool, old) in &existing {
        let Some(op) = table.find(tool) else { continue };
        let mut expected = old.policy.clone();
        expected.execute_gated |= table.is_write(op);
        if expected != op.policy {
            problems.push(format!(
                "{tool}: policy {:?} != {:?}",
                old.policy, op.policy
            ));
        }
        let old_vars: Vec<(&str, bool)> = old
            .fields
            .iter()
            .filter(|(f, _)| f.as_str() != "endpoint" && !OVERRIDE_FIELDS.contains(&f.as_str()))
            .map(|(f, v)| (f.as_str(), v.required))
            .collect();
        let mut new_vars: Vec<(&str, bool)> = op
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.required))
            .collect();
        new_vars.sort();
        if old_vars != new_vars {
            problems.push(format!("{tool}: variables {old_vars:?} != {new_vars:?}"));
        }
    }
    (problems, extra)
}

#[test]
fn graphql_fixture_matches_surface_codegen() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/surface_table/graphql");
    let (problems, extra) = diff(&dir.join("schemas"), &dir.join("queries"), "nas");
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert_eq!(
        extra,
        BTreeSet::from(["nas.delete_key".to_string(), "nas.vms".to_string()])
    );
}

#[test]
#[ignore = "needs ORCA_GOLDEN_UNRAID pointing at the unraid plugin checkout"]
fn full_unraid_matches_surface_codegen() {
    let dir = PathBuf::from(std::env::var("ORCA_GOLDEN_UNRAID").unwrap());
    let (problems, extra) = diff(&dir.join("schemas"), &dir.join("queries"), "unraid");
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    println!("unraid extra: {} {extra:?}", extra.len());
}
