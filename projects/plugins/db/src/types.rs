//! Wire types for the `namespace.schema` + `namespace.schema.view` tools.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Registry CRUD types ─────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaDbEntry {
    pub name: String,
    pub driver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub user: String,
    pub database: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domains_file: Option<String>,
    pub enabled: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasOutput {
    pub schemas: Vec<SchemaDbEntry>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddSchemaArgs {
    pub name: String,
    pub database: String,
    pub user: String,
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains_file: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaMutationResult {
    pub name: String,
    pub changed: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RemoveSchemaArgs {
    pub name: String,
}

// ── Schema view types ───────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaArgs {}

/// One row in `tabs[*].tables`.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaTableInfo {
    pub name: String,
    pub comment: String,
}

/// One column entry within `tabs[*].columns[tableName]`. Field names match
/// the HTTP `/api/schema` payload — the frontend reads `fk_target`
/// snake_case directly.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub nullable: bool,
    pub key: String,
    pub extra: String,
    pub fk_target: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaForeignKey {
    pub table: String,
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
}

/// Domain grouping (loaded from each schema DB's `domainsFile` JSON).
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaDomain {
    pub key: String,
    pub label: String,
    pub color: String,
    pub tables: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subgroup: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaTab {
    pub title: String,
    pub tables: Vec<SchemaTableInfo>,
    pub columns: HashMap<String, Vec<SchemaColumn>>,
    pub foreign_keys: Vec<SchemaForeignKey>,
    pub domains: Vec<SchemaDomain>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetSchemaOutput {
    pub tabs: Vec<SchemaTab>,
    pub show_tabs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsOutput {
    pub domains: Vec<SchemaDomain>,
}
