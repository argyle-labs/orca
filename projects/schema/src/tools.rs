//! `namespace.schema` CRUD + `namespace.schema.view` tools. CRUD reaches
//! straight into `db::schema_databases::*`; view tools call into [`crate::view`].

use orca_macro::orca_tool;

use crate::types::{
    AddSchemaArgs, GetSchemaArgs, GetSchemaDomainsArgs, GetSchemaDomainsOutput, GetSchemaOutput,
    ListSchemasArgs, ListSchemasOutput, RemoveSchemaArgs, SchemaDbEntry, SchemaMutationResult,
};
use crate::view;

// ── Registry CRUD ───────────────────────────────────────────────────────────

/// List all MySQL/MariaDB/Postgres/SQLite schema databases registered in orca.db.
#[orca_tool(domain = "namespace.schema", verb = "list")]
async fn list_schemas(
    _args: ListSchemasArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListSchemasOutput> {
    let conn = db::open_default()?;
    let schemas = db::schema_databases::list(&conn)?
        .into_iter()
        .map(|d| SchemaDbEntry {
            name: d.name,
            driver: d.driver,
            host: d.host,
            port: d.port,
            user: d.user,
            database: d.database,
            container: d.container,
            domains_file: d.domains_file,
            enabled: d.enabled,
        })
        .collect();
    Ok(ListSchemasOutput { schemas })
}

/// [MUTATES STATE] Add or update a schema database in orca.db. Use container OR host/port, not both.
#[orca_tool(domain = "namespace.schema", verb = "create")]
async fn add_schema(
    args: AddSchemaArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SchemaMutationResult> {
    let row = db::schema_databases::SchemaDbRow {
        name: args.name.clone(),
        driver: "mysql".to_string(),
        host: args.host,
        port: args.port,
        user: args.user,
        password: args.password,
        database: args.database,
        container: args.container,
        domains_file: args.domains_file,
        enabled: true,
    };
    let conn = db::open_default()?;
    db::schema_databases::upsert(&conn, &row)?;
    Ok(SchemaMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a schema database from orca.db by name.
#[orca_tool(domain = "namespace.schema", verb = "delete")]
async fn remove_schema(
    args: RemoveSchemaArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SchemaMutationResult> {
    let conn = db::open_default()?;
    let changed = db::schema_databases::remove(&conn, &args.name)?;
    Ok(SchemaMutationResult {
        name: args.name,
        changed,
    })
}

// ── Schema view ─────────────────────────────────────────────────────────────

/// Return the multi-tab schema view across every configured database. Result is `{ tabs, showTabs, errors? }`.
#[orca_tool(domain = "namespace.schema.view", verb = "detail")]
async fn schema_view_detail(
    _args: GetSchemaArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetSchemaOutput> {
    view::build_schema_response()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// Return the flattened list of domain definitions across every configured database.
#[orca_tool(domain = "namespace.schema.view", verb = "list")]
async fn schema_view_list(
    _args: GetSchemaDomainsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetSchemaDomainsOutput> {
    Ok(GetSchemaDomainsOutput {
        domains: view::build_schema_domains(),
    })
}
