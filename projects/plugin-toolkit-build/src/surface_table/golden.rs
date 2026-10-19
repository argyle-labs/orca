#![allow(clippy::disallowed_types)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ::openapi::surface::{ApiSurface, ParamIn, Policy};
use serde_json::{Map, Value};

use super::openapi::{GetLists, Options, build, parse_list};

#[path = "../../tests/fixtures/surface_table/proxmox_ungate.rs"]
#[allow(dead_code, clippy::all)]
mod ungate;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Field {
    pub(super) required: bool,
    kind: Option<&'static str>,
}

#[derive(Debug)]
pub(super) struct Tool {
    pub(super) fields: BTreeMap<String, Field>,
    pub(super) policy: Policy,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/surface_table")
}

fn existing_surface(raw: &str, flavor: &str, lists: Option<&GetLists>) -> String {
    let tmp = std::env::temp_dir().join(format!(
        "surface-golden-{flavor}-{}-{}",
        std::process::id(),
        raw.len()
    ));
    let specs = tmp.join("specs");
    std::fs::create_dir_all(&specs).unwrap();
    std::fs::write(specs.join(format!("{flavor}.openapi.json")), raw).unwrap();
    let codegen =
        crate::openapi::codegen_one(raw, flavor, flavor, None, Default::default()).unwrap();
    std::fs::write(tmp.join(format!("{flavor}_codegen.rs")), codegen).unwrap();
    crate::surface::openapi::generate(&specs, &tmp, flavor).unwrap();
    let src = std::fs::read_to_string(tmp.join(format!("{flavor}_surface.rs"))).unwrap();
    std::fs::remove_dir_all(&tmp).unwrap();
    match lists {
        Some(l) => {
            let out = ungate::classify_gets(&src, &l.allow, &l.gated);
            assert_eq!(out.rewritten, out.gets);
            out.source
        }
        None => src,
    }
}

fn rust_kind(ty: &str) -> Option<&'static str> {
    let ty = ty.replace(' ', "");
    let inner = ty
        .strip_prefix("Option<")
        .and_then(|t| t.strip_suffix('>'))
        .unwrap_or(&ty);
    if inner.starts_with("Vec<") {
        return Some("array");
    }
    match inner {
        "String" => Some("string"),
        "bool" => Some("boolean"),
        "f64" => Some("number"),
        "i64" | "u64" | "i32" | "u32" | "u16" => Some("integer"),
        t if t.ends_with("Timestamp") => Some("string"),
        _ => None,
    }
}

fn schema_kind(schema: &Value, defs: &Map<String, Value>) -> Option<&'static str> {
    let schema = match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => defs.get(r.rsplit('/').next()?)?,
        None => schema,
    };
    if schema.get("enum").is_some() {
        return None;
    }
    match schema.get("type")?.as_str()? {
        "string" => Some("string"),
        "boolean" => Some("boolean"),
        "number" => Some("number"),
        "integer" => Some("integer"),
        "array" => Some("array"),
        _ => None,
    }
}

pub(super) fn existing_tools(src: &str, flavor: &str) -> BTreeMap<String, Tool> {
    let file = syn::parse_file(src).unwrap();
    let mut structs: BTreeMap<String, BTreeMap<String, Field>> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for item in &file.items {
        if let syn::Item::Struct(s) = item {
            let fields = s
                .fields
                .iter()
                .map(|f| {
                    let ty = quote::ToTokens::to_token_stream(&f.ty).to_string();
                    (
                        f.ident.as_ref().unwrap().to_string(),
                        Field {
                            required: !ty.replace(' ', "").starts_with("Option<"),
                            kind: rust_kind(&ty),
                        },
                    )
                })
                .collect();
            structs.insert(s.ident.to_string(), fields);
        }
    }
    for item in &file.items {
        let syn::Item::Fn(f) = item else { continue };
        let attr = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("orca_tool"))
            .unwrap();
        let mut kv: BTreeMap<String, String> = BTreeMap::new();
        attr.parse_nested_meta(|meta| {
            let key = meta.path.get_ident().unwrap().to_string();
            let lit: syn::Lit = meta.value()?.parse()?;
            let val = match lit {
                syn::Lit::Str(s) => s.value(),
                syn::Lit::Bool(b) => b.value.to_string(),
                _ => unreachable!(),
            };
            kv.insert(key, val);
            Ok(())
        })
        .unwrap();
        let verb = kv["verb"].clone();
        let read_shaped = verb_shape::is_read_shaped(&verb);
        let role = kv
            .get("role")
            .cloned()
            .unwrap_or_else(|| if read_shaped { "any" } else { "admin" }.to_string());
        let policy = Policy {
            role: if role == "any" { "read".into() } else { role },
            execute_gated: kv
                .get("execute_gated")
                .map_or(!read_shaped, |v| v == "true"),
            data_mutation: kv.get("data_mutation").is_some_and(|v| v == "true"),
            local_only: false,
        };
        let fields = structs.remove(&format!("SurfaceArgs_{verb}")).unwrap();
        out.insert(format!("{flavor}.{verb}"), Tool { fields, policy });
    }
    out
}

fn table_tools(table: &ApiSurface) -> BTreeMap<String, Tool> {
    table
        .ops
        .iter()
        .filter(|op| !op.internal)
        .map(|op| {
            let mut fields = BTreeMap::from([(
                "endpoint".to_string(),
                Field {
                    required: true,
                    kind: Some("string"),
                },
            )]);
            for p in &op.params {
                fields.insert(
                    p.name.clone(),
                    Field {
                        required: p.required || p.location == ParamIn::Path,
                        kind: schema_kind(&p.schema, &table.defs),
                    },
                );
            }
            (
                op.tool.clone(),
                Tool {
                    fields,
                    policy: op.policy.clone(),
                },
            )
        })
        .collect()
}

fn diff(raw: &str, flavor: &str, lists: Option<&GetLists>) -> (Vec<String>, BTreeSet<String>) {
    let existing = existing_tools(&existing_surface(raw, flavor, lists), flavor);
    let (table, _) = build(raw, flavor, &Options::default(), lists).unwrap();
    let ours = table_tools(&table);
    let mut problems = Vec::new();
    for op in &table.ops {
        let write = op.method != "GET";
        if write && (op.policy.role != "admin" || !op.policy.execute_gated) {
            problems.push(format!(
                "{}: write is not admin+gated: {:?}",
                op.tool, op.policy
            ));
        }
        if !existing.contains_key(&op.tool) && !op.internal {
            let allowed = lists.is_some_and(|l| l.allow.contains(&op.path));
            let gated = write || lists.is_none_or(|l| l.gated.contains(&op.path));
            let role = if allowed { "read" } else { "admin" };
            if op.policy.role != role || op.policy.execute_gated != gated {
                problems.push(format!("{}: new tool policy {:?}", op.tool, op.policy));
            }
        }
    }
    for (name, old) in &existing {
        let Some(new) = ours.get(name) else {
            problems.push(format!("{name}: missing from table"));
            continue;
        };
        let mut expected = old.policy.clone();
        expected.execute_gated |= table.find(name).is_some_and(|op| op.method != "GET");
        if expected != new.policy {
            problems.push(format!(
                "{name}: policy {:?} != {:?}",
                old.policy, new.policy
            ));
        }
        let old_names: BTreeSet<_> = old.fields.keys().collect();
        let new_names: BTreeSet<_> = new.fields.keys().collect();
        if old_names != new_names {
            problems.push(format!("{name}: fields {old_names:?} != {new_names:?}"));
            continue;
        }
        for (field, of) in &old.fields {
            let nf = &new.fields[field];
            let kinds_differ = of.kind.is_some() && nf.kind.is_some() && of.kind != nf.kind;
            let required_differs = of.required != nf.required && field != "body";
            if required_differs || kinds_differ {
                problems.push(format!("{name}.{field}: {of:?} != {nf:?}"));
            }
        }
    }
    let extra = ours
        .keys()
        .filter(|n| !existing.contains_key(*n))
        .cloned()
        .collect();
    (problems, extra)
}

fn assert_golden(raw: &str, flavor: &str, lists: Option<&GetLists>) -> BTreeSet<String> {
    let (problems, extra) = diff(raw, flavor, lists);
    assert!(problems.is_empty(), "{flavor}:\n{}", problems.join("\n"));
    extra
}

fn proxmox_lists(dir: &Path) -> GetLists {
    GetLists {
        allow: parse_list(&std::fs::read_to_string(dir.join("read_allowlist.txt")).unwrap()),
        gated: parse_list(&std::fs::read_to_string(dir.join("gated_gets.txt")).unwrap()),
    }
}

#[test]
fn proxmox_fixture_matches_surface_codegen() {
    let dir = fixture_dir();
    let raw = std::fs::read_to_string(dir.join("proxmox.openapi.json")).unwrap();
    let extra = assert_golden(&raw, "proxmox", Some(&proxmox_lists(&dir)));
    assert_eq!(
        extra,
        BTreeSet::from([
            "proxmox.delete_delete_rule_cluster_firewall_groups_group_pos".to_string()
        ])
    );
}

#[test]
fn gitea_fixture_matches_surface_codegen() {
    let raw = std::fs::read_to_string(fixture_dir().join("gitea.openapi.json")).unwrap();
    let extra = assert_golden(&raw, "gitea", None);
    assert!(extra.is_empty(), "{extra:?}");
}

#[test]
#[ignore = "needs ORCA_GOLDEN_PROXMOX_SPECS / ORCA_GOLDEN_GITEA_SPECS pointing at plugin specs dirs"]
fn full_specs_match_surface_codegen() {
    if let Ok(dir) = std::env::var("ORCA_GOLDEN_PROXMOX_SPECS") {
        let dir = PathBuf::from(dir);
        let raw = std::fs::read_to_string(dir.join("proxmox.openapi.json")).unwrap();
        let lists = proxmox_lists(&dir);
        let extra = assert_golden(&raw, "proxmox", Some(&lists));
        let (table, _) = build(&raw, "proxmox", &Options::default(), Some(&lists)).unwrap();
        let bytes = serde_json::to_string(&table).unwrap().len();
        println!(
            "proxmox: {} ops, {bytes} bytes, extra {extra:?}",
            table.ops.len()
        );
        assert!(bytes <= plugin_proto::MAX_API_SURFACE_BYTES);
    }
    if let Ok(dir) = std::env::var("ORCA_GOLDEN_GITEA_SPECS") {
        let raw = std::fs::read_to_string(PathBuf::from(dir).join("gitea.openapi.json")).unwrap();
        let extra = assert_golden(&raw, "gitea", None);
        let (table, _) = build(&raw, "gitea", &Options::default(), None).unwrap();
        let bytes = serde_json::to_string(&table).unwrap().len();
        println!(
            "gitea: {} ops, {bytes} bytes, extra {extra:?}",
            table.ops.len()
        );
    }
}
