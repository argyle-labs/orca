use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use utoipa::ToSchema;

pub mod php_parse;
pub mod ci4_generator;
pub mod ci2_generator;
pub mod nextjs_generator;

pub fn openapi_dir() -> PathBuf {
    // Override with ORCA_OPENAPI_DIR for non-standard installs.
    if let Ok(custom) = std::env::var("ORCA_OPENAPI_DIR") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".orca/openapi/specs")
}

/// Registry entry for a tracked external API spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecEntry {
    pub repo: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// "manual" or "snapshot" (snapshot not yet implemented)
    pub source: String,
    #[serde(rename = "baseUrl", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(rename = "capturedAt", skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
}

pub struct SpecRegistry {
    pub entries: Vec<SpecEntry>,
}

impl SpecRegistry {
    pub fn load() -> Result<Self> {
        let path = openapi_dir().join("registry.json");
        let entries = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(Self { entries })
    }

    pub fn save(&self) -> Result<()> {
        let dir = openapi_dir();
        std::fs::create_dir_all(&dir)?;
        let raw = serde_json::to_string_pretty(&self.entries)?;
        std::fs::write(dir.join("registry.json"), raw)?;
        Ok(())
    }

    /// Register an entry and scaffold both spec files if they don't exist yet.
    /// Returns the path to the full spec file.
    pub fn add(&mut self, entry: SpecEntry) -> Result<PathBuf> {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.repo == entry.repo) {
            *existing = entry.clone();
        } else {
            self.entries.push(entry.clone());
        }
        self.save()?;

        let dir = openapi_dir();
        let full_path = dir.join(format!("{}.json", entry.repo));
        let public_path = dir.join(format!("{}.public.json", entry.repo));

        if !full_path.exists() {
            let scaffold = scaffold_full_spec(&entry);
            std::fs::write(&full_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        if !public_path.exists() {
            let scaffold = scaffold_public_spec(&entry);
            std::fs::write(&public_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        Ok(full_path)
    }
}

fn base_spec_info(entry: &SpecEntry, title_suffix: &str) -> Value {
    let now = chrono::Utc::now().to_rfc3339();
    let captured = entry.captured_at.as_deref().unwrap_or(&now);
    let servers = entry
        .base_url
        .as_ref()
        .map(|u| json!([{ "url": u, "description": "Production" }]))
        .unwrap_or(json!([]));
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": format!("{}{}", entry.repo, title_suffix),
            "version": "0.0.0",
            "description": entry.description.as_deref().unwrap_or("")
        },
        "x-orca": {
            "repo": entry.repo,
            "project": entry.project,
            "source": entry.source,
            "baseUrl": entry.base_url,
            "capturedAt": captured
        },
        "servers": servers,
        "paths": {},
        "components": { "schemas": {}, "securitySchemes": {} }
    })
}

/// Full internal spec scaffold — all endpoints, internal + public.
pub fn scaffold_full_spec(entry: &SpecEntry) -> Value {
    let mut spec = base_spec_info(entry, "");
    spec["tags"] = json!([
        { "name": "public",   "description": "Publicly accessible endpoints" },
        { "name": "internal", "description": "Internal endpoints — not for external consumers" }
    ]);
    spec
}

/// Standalone public spec scaffold — complete, self-contained, public endpoints only.
/// This is NOT a filtered derivative — it is independently maintained.
pub fn scaffold_public_spec(entry: &SpecEntry) -> Value {
    let mut spec = base_spec_info(entry, " (Public API)");
    spec["tags"] = json!([
        { "name": "public", "description": "Publicly accessible endpoints" }
    ]);
    spec
}

const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Domain tags in orca's own spec that are publicly accessible.
/// utoipa 4.x only supports one tag per path, so we classify by domain name.
const BRAIN_PUBLIC_DOMAINS: &[&str] = &["docs", "library"];

fn filter_ops(mut spec: Value, keep: impl Fn(&Value) -> bool) -> Value {
    if let Some(paths) = spec["paths"].as_object_mut() {
        let keys: Vec<String> = paths.keys().cloned().collect();
        for key in &keys {
            if let Some(item) = paths.get_mut(key).and_then(|v| v.as_object_mut()) {
                for method in METHODS {
                    if let Some(op) = item.get(*method)
                        && !keep(op)
                    {
                        item.remove(*method);
                    }
                }
            }
        }
        let empty: Vec<String> = paths
            .iter()
            .filter(|(_, v)| !METHODS.iter().any(|m| v.get(m).is_some()))
            .map(|(k, _)| k.clone())
            .collect();
        for p in empty {
            paths.remove(&p);
        }
    }
    spec
}

/// Filter orca's own spec to only operations in publicly accessible domain groups.
/// Uses domain tags (docs, library) since utoipa 4.x doesn't support multi-tag paths.
pub fn filter_brain_public(spec: Value) -> Value {
    let mut filtered = filter_ops(spec, |op| {
        op["tags"]
            .as_array()
            .map(|tags| {
                tags.iter()
                    .any(|t| BRAIN_PUBLIC_DOMAINS.contains(&t.as_str().unwrap_or("")))
            })
            .unwrap_or(false)
    });

    // Collect tags actually referenced in the surviving paths.
    let used_tags: std::collections::HashSet<String> = filtered["paths"]
        .as_object()
        .into_iter()
        .flat_map(|paths| paths.values())
        .flat_map(|item| METHODS.iter().filter_map(|m| item.get(*m)))
        .flat_map(|op| op["tags"].as_array().into_iter().flatten())
        .filter_map(|t| t.as_str().map(String::from))
        .collect();

    if let Some(tags) = filtered["tags"].as_array() {
        let pruned: Vec<Value> = tags
            .iter()
            .filter(|t| {
                t["name"]
                    .as_str()
                    .map(|n| used_tags.contains(n))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        filtered["tags"] = Value::Array(pruned);
    }

    filtered
}

// ── GraphQL schema parser ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GraphQlField {
    pub name: String,
    #[serde(rename = "typeName")]
    pub type_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GraphQlOperation {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub args: Vec<GraphQlField>,
    pub returns: String,
    pub deprecated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GraphQlType {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<GraphQlField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GraphQlEnum {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GraphQlInfo {
    pub repo: String,
    pub queries: Vec<GraphQlOperation>,
    pub mutations: Vec<GraphQlOperation>,
    pub subscriptions: Vec<GraphQlOperation>,
    pub types: Vec<GraphQlType>,
    pub inputs: Vec<GraphQlType>,
    pub enums: Vec<GraphQlEnum>,
}

fn gql_type_str(t: &graphql_parser::schema::Type<String>) -> (String, bool) {
    use graphql_parser::schema::Type;
    match t {
        Type::NonNullType(inner) => {
            let (s, _) = gql_type_str(inner);
            (s, true)
        }
        Type::ListType(inner) => {
            let (s, _) = gql_type_str(inner);
            (format!("[{s}]"), false)
        }
        Type::NamedType(n) => (n.clone(), false),
    }
}

fn map_field(f: &graphql_parser::schema::Field<String>) -> GraphQlField {
    let (type_name, required) = gql_type_str(&f.field_type);
    GraphQlField {
        name: f.name.clone(),
        type_name,
        description: f.description.clone(),
        required,
    }
}

fn map_input_field(f: &graphql_parser::schema::InputValue<String>) -> GraphQlField {
    let (type_name, required) = gql_type_str(&f.value_type);
    GraphQlField {
        name: f.name.clone(),
        type_name,
        description: f.description.clone(),
        required,
    }
}

fn map_operation(f: &graphql_parser::schema::Field<String>) -> GraphQlOperation {
    let (returns, _) = gql_type_str(&f.field_type);
    let deprecated = f.directives.iter().any(|d| d.name == "deprecated");
    GraphQlOperation {
        name: f.name.clone(),
        description: f.description.clone(),
        args: f.arguments.iter().map(map_input_field).collect(),
        returns,
        deprecated,
    }
}

/// Parse a GraphQL **operations document** (named queries/mutations/subscriptions with selection
/// sets) into `GraphQlInfo`. Used for client operation files like `rebuy-shopify-client.graphql`.
pub fn parse_graphql_operations(repo: &str, src: &str) -> Result<GraphQlInfo> {
    use graphql_parser::query::{Definition, OperationDefinition, parse_query};

    fn op_type_str(t: &graphql_parser::query::Type<String>) -> (String, bool) {
        use graphql_parser::query::Type;
        match t {
            Type::NonNullType(inner) => {
                let (s, _) = op_type_str(inner);
                (s, true)
            }
            Type::ListType(inner) => {
                let (s, _) = op_type_str(inner);
                (format!("[{s}]"), false)
            }
            Type::NamedType(n) => (n.clone(), false),
        }
    }

    let doc = parse_query::<String>(src)
        .map_err(|e| anyhow::anyhow!("GraphQL parse error: {e}"))?;

    let mut queries = Vec::new();
    let mut mutations = Vec::new();
    let mut subscriptions = Vec::new();

    for def in &doc.definitions {
        let Definition::Operation(op) = def else { continue };
        let (name, vars, bucket) = match op {
            OperationDefinition::Query(q) => (
                q.name.clone().unwrap_or_else(|| "anonymous".into()),
                &q.variable_definitions,
                &mut queries,
            ),
            OperationDefinition::Mutation(m) => (
                m.name.clone().unwrap_or_else(|| "anonymous".into()),
                &m.variable_definitions,
                &mut mutations,
            ),
            OperationDefinition::Subscription(s) => (
                s.name.clone().unwrap_or_else(|| "anonymous".into()),
                &s.variable_definitions,
                &mut subscriptions,
            ),
            OperationDefinition::SelectionSet(_) => continue,
        };
        let args: Vec<GraphQlField> = vars
            .iter()
            .map(|v| {
                let (type_name, required) = op_type_str(&v.var_type);
                GraphQlField { name: v.name.clone(), type_name, description: None, required }
            })
            .collect();
        bucket.push(GraphQlOperation {
            name,
            description: None,
            args,
            returns: String::new(),
            deprecated: false,
        });
    }

    Ok(GraphQlInfo {
        repo: repo.to_string(),
        queries,
        mutations,
        subscriptions,
        types: vec![],
        inputs: vec![],
        enums: vec![],
    })
}

/// Parse a GraphQL SDL string into a structured `GraphQlInfo`.
/// Auto-detects format: schema SDL (`type Query { ... }`) vs operation document
/// (`mutation Foo(...) { ... }`). Falls back to operation parsing if SDL parse fails.
pub fn parse_graphql_sdl(repo: &str, sdl: &str) -> Result<GraphQlInfo> {
    use graphql_parser::schema::{Definition, TypeDefinition, parse_schema};

    // Detect operation documents by presence of named operations without type definitions.
    // If SDL parse fails, try operations parser before returning the error.
    let schema_result = parse_schema::<String>(sdl);
    let doc = match schema_result {
        Ok(d) => d,
        Err(_) => return parse_graphql_operations(repo, sdl),
    };

    // SDL parsed — but it may be an operations file that happened to parse (unlikely).
    // Check if it has any type definitions; if not, treat as operations.
    let has_type_defs = doc.definitions.iter().any(|d| {
        matches!(d, Definition::TypeDefinition(_) | Definition::SchemaDefinition(_))
    });
    if !has_type_defs {
        return parse_graphql_operations(repo, sdl);
    }

    let mut queries = Vec::new();
    let mut mutations = Vec::new();
    let mut subscriptions = Vec::new();
    let mut types = Vec::new();
    let mut inputs = Vec::new();
    let mut enums = Vec::new();

    for def in &doc.definitions {
        match def {
            Definition::TypeDefinition(td) => match td {
                TypeDefinition::Object(obj) => match obj.name.as_str() {
                    "Query" => queries = obj.fields.iter().map(map_operation).collect(),
                    "Mutation" => mutations = obj.fields.iter().map(map_operation).collect(),
                    "Subscription" => subscriptions = obj.fields.iter().map(map_operation).collect(),
                    _ => types.push(GraphQlType {
                        name: obj.name.clone(),
                        description: obj.description.clone(),
                        fields: obj.fields.iter().map(map_field).collect(),
                    }),
                },
                TypeDefinition::InputObject(inp) => inputs.push(GraphQlType {
                    name: inp.name.clone(),
                    description: inp.description.clone(),
                    fields: inp.fields.iter().map(map_input_field).collect(),
                }),
                TypeDefinition::Enum(e) => enums.push(GraphQlEnum {
                    name: e.name.clone(),
                    description: e.description.clone(),
                    values: e.values.iter().map(|v| v.name.clone()).collect(),
                }),
                _ => {}
            },
            _ => {}
        }
    }

    Ok(GraphQlInfo { repo: repo.to_string(), queries, mutations, subscriptions, types, inputs, enums })
}
