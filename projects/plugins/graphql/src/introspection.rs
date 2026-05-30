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
/// sets) into `GraphQlInfo`. Used for client operation files like `acme-shopify-client.graphql`.
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

#[cfg(test)]
mod tests {
    use super::*;

    const SDL: &str = r#"
        "A user account."
        type User {
            id: ID!
            name: String
            tags: [String!]
        }

        "Input for creating a user."
        input NewUser {
            name: String!
            age: Int
        }

        "Auth role."
        enum Role { ADMIN GUEST }

        type Query {
            "Look up a user by id."
            user(id: ID!): User
            users: [User!]!
        }

        type Mutation {
            createUser(input: NewUser!): User @deprecated(reason: "use createAccount")
        }

        type Subscription {
            userCreated: User
        }
    "#;

    #[test]
    fn parse_graphql_sdl_extracts_every_definition_kind() {
        let info = parse_graphql_sdl("repo", SDL).unwrap();
        assert_eq!(info.repo, "repo");
        assert_eq!(info.queries.len(), 2);
        assert_eq!(info.mutations.len(), 1);
        assert_eq!(info.subscriptions.len(), 1);
        assert_eq!(info.types.len(), 1);
        assert_eq!(info.inputs.len(), 1);
        assert_eq!(info.enums.len(), 1);

        let user_q = info.queries.iter().find(|o| o.name == "user").unwrap();
        assert_eq!(user_q.description.as_deref(), Some("Look up a user by id."));
        assert_eq!(user_q.returns, "User");
        assert_eq!(user_q.args.len(), 1);
        assert_eq!(user_q.args[0].name, "id");
        assert_eq!(user_q.args[0].type_name, "ID");
        assert!(user_q.args[0].required);

        let users_q = info.queries.iter().find(|o| o.name == "users").unwrap();
        // NonNull list of NonNull strings: outer type renders as "[User]" (NonNull peels into list).
        assert_eq!(users_q.returns, "[User]");

        let mutation = &info.mutations[0];
        assert!(mutation.deprecated);

        let user_type = &info.types[0];
        assert_eq!(user_type.name, "User");
        assert_eq!(user_type.description.as_deref(), Some("A user account."));
        let id_field = user_type.fields.iter().find(|f| f.name == "id").unwrap();
        assert_eq!(id_field.type_name, "ID");
        assert!(id_field.required);
        let name_field = user_type.fields.iter().find(|f| f.name == "name").unwrap();
        assert!(!name_field.required);
        let tags_field = user_type.fields.iter().find(|f| f.name == "tags").unwrap();
        assert_eq!(tags_field.type_name, "[String]");

        let input = &info.inputs[0];
        assert_eq!(input.name, "NewUser");
        assert_eq!(input.fields.len(), 2);
        let name_in = input.fields.iter().find(|f| f.name == "name").unwrap();
        assert!(name_in.required);

        let role = &info.enums[0];
        assert_eq!(role.name, "Role");
        assert_eq!(role.values, vec!["ADMIN", "GUEST"]);
    }

    #[test]
    fn parse_graphql_operations_buckets_query_mutation_subscription_and_anonymous() {
        let src = r#"
            query GetUser($id: ID!) { user(id: $id) { name } }
            mutation Create($name: String!) { createUser(name: $name) { id } }
            subscription Live { userCreated { id } }
            query { anonymousField }
            { bareSelectionSet }
        "#;
        let info = parse_graphql_operations("r", src).unwrap();
        assert_eq!(
            info.queries
                .iter()
                .map(|q| q.name.as_str())
                .collect::<Vec<_>>(),
            vec!["GetUser", "anonymous"]
        );
        assert_eq!(info.mutations[0].name, "Create");
        assert_eq!(info.subscriptions[0].name, "Live");

        let q = &info.queries[0];
        assert_eq!(q.args.len(), 1);
        assert_eq!(q.args[0].name, "id");
        assert_eq!(q.args[0].type_name, "ID");
        assert!(q.args[0].required);
        assert!(q.args[0].description.is_none());
        assert_eq!(q.returns, "");
        assert!(!q.deprecated);
    }

    #[test]
    fn parse_graphql_operations_handles_list_var_type() {
        let src = "query Q($ids: [ID!]) { x }";
        let info = parse_graphql_operations("r", src).unwrap();
        assert_eq!(info.queries[0].args[0].type_name, "[ID]");
        assert!(!info.queries[0].args[0].required);
    }

    #[test]
    fn parse_graphql_operations_propagates_parse_errors() {
        let err = parse_graphql_operations("r", "this is not graphql {").unwrap_err();
        assert!(err.to_string().contains("GraphQL parse error"));
    }

    #[test]
    fn parse_graphql_sdl_falls_back_to_operations_when_schema_parse_fails() {
        // Operations-only document — schema parser typically rejects this.
        let src = "query Foo { x }";
        let info = parse_graphql_sdl("r", src).unwrap();
        assert_eq!(info.queries.len(), 1);
        assert_eq!(info.queries[0].name, "Foo");
    }
}
