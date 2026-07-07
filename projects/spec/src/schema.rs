//! `schema.*` + `schema.view.*` tools. Schema databases (MySQL/Postgres/SQLite)
//! are first-class objects registered in orca.db that assign to a namespace.
//! Tools call into `db::schema` for the heavy introspection +
//! tabbed view rendering.

use derive::orca_tool;

use db::schema::types::{
    AddSchemaArgs, GetSchemaArgs, GetSchemaOutput, ListSchemasArgs, ListSchemasOutput,
    RemoveSchemaArgs, SchemaDbEntry, SchemaMutationResult,
};
use db::schema::view;

#[orca_tool(domain = "schema", verb = "list")]
async fn list_schemas(
    _args: ListSchemasArgs,
    _ctx: &contract::ToolCtx,
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
#[orca_tool(domain = "schema", verb = "create")]
async fn add_schema(
    args: AddSchemaArgs,
    _ctx: &contract::ToolCtx,
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
#[orca_tool(domain = "schema", verb = "delete")]
async fn remove_schema(
    args: RemoveSchemaArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SchemaMutationResult> {
    let conn = db::open_default()?;
    let changed = db::schema_databases::remove(&conn, &args.name)?;
    Ok(SchemaMutationResult {
        name: args.name,
        changed,
    })
}

/// Multi-tab introspection across every configured database. Result is
/// `{ tabs, showTabs, errors?, domains }` — full schema view including the
/// flattened domain list that the old `schema.view.list` returned separately.
#[orca_tool(domain = "schema", verb = "detail")]
async fn schema_detail(
    _args: GetSchemaArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<GetSchemaOutput> {
    view::build_schema_response()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}
