//! GraphQL SDL + operations document scanner. Parses `.graphql` files into
//! a structured [`GraphQlInfo`].
//!
//! These types describe the parsed schema shape and are not persisted —
//! they're returned directly to namespace.spec.graphql.detail callers.

use anyhow::Result;
use graphql_parser::query::Type as GqlType;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphQlField {
    pub name: String,
    #[serde(rename = "typeName")]
    pub type_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct GraphQlOperation {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub args: Vec<GraphQlField>,
    pub returns: String,
    pub deprecated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct GraphQlType {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<GraphQlField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct GraphQlEnum {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, JsonSchema)]
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
        match t {
            GqlType::NonNullType(inner) => {
                let (s, _) = op_type_str(inner);
                (s, true)
            }
            GqlType::ListType(inner) => {
                let (s, _) = op_type_str(inner);
                (format!("[{s}]"), false)
            }
            GqlType::NamedType(n) => (n.clone(), false),
        }
    }

    let doc =
        parse_query::<String>(src).map_err(|e| anyhow::anyhow!("GraphQL parse error: {e}"))?;

    let mut queries = Vec::new();
    let mut mutations = Vec::new();
    let mut subscriptions = Vec::new();

    for def in &doc.definitions {
        let Definition::Operation(op) = def else {
            continue;
        };
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
                GraphQlField {
                    name: v.name.clone(),
                    type_name,
                    description: None,
                    required,
                }
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
/// Auto-detects format: schema SDL vs operation document. Falls back to
/// operation parsing if SDL parse fails or has no type defs.
pub fn parse_graphql_sdl(repo: &str, sdl: &str) -> Result<GraphQlInfo> {
    use graphql_parser::schema::{Definition, TypeDefinition, parse_schema};

    let schema_result = parse_schema::<String>(sdl);
    let doc = match schema_result {
        Ok(d) => d,
        Err(_) => return parse_graphql_operations(repo, sdl),
    };

    let has_type_defs = doc.definitions.iter().any(|d| {
        matches!(
            d,
            Definition::TypeDefinition(_) | Definition::SchemaDefinition(_)
        )
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
        if let Definition::TypeDefinition(td) = def {
            match td {
                TypeDefinition::Object(obj) => match obj.name.as_str() {
                    "Query" => queries = obj.fields.iter().map(map_operation).collect(),
                    "Mutation" => mutations = obj.fields.iter().map(map_operation).collect(),
                    "Subscription" => {
                        subscriptions = obj.fields.iter().map(map_operation).collect()
                    }
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
            }
        }
    }

    Ok(GraphQlInfo {
        repo: repo.to_string(),
        queries,
        mutations,
        subscriptions,
        types,
        inputs,
        enums,
    })
}
