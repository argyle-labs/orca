//! Doc root + ignore pattern registry tools. Backed directly by
//! `db::docs::*` — no service-trait indirection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;

// ── Doc roots ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootRegEntry {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsOutput {
    pub roots: Vec<DocRootRegEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddDocRootArgs {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveDocRootArgs {
    pub name: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsOutput {
    pub patterns: Vec<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternArgs {
    pub pattern: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternMutationResult {
    pub pattern: String,
    pub changed: bool,
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// List all documentation roots registered in orca.db.
#[cfg(feature = "native")]
#[orca_tool(domain = "namespace.doc.root", verb = "list")]
async fn list_doc_roots(
    _args: ListDocRootsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListDocRootsOutput> {
    let conn = db::open_default()?;
    let roots = db::docs::list_roots(&conn)?
        .into_iter()
        .map(|r| DocRootRegEntry {
            name: r.name,
            path: r.path,
            description: r.description,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListDocRootsOutput { roots })
}

/// [MUTATES STATE] Register a documentation root directory in orca.db.
#[cfg(feature = "native")]
#[orca_tool(domain = "namespace.doc.root", verb = "create")]
async fn add_doc_root(
    args: AddDocRootArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DocRootMutationResult> {
    let row = db::docs::RootRow {
        name: args.name.clone(),
        path: args.path,
        description: args.description,
        enabled: true,
    };
    let conn = db::open_default()?;
    db::docs::upsert_root(&conn, &row)?;
    Ok(DocRootMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a documentation root from orca.db by name.
#[cfg(feature = "native")]
#[orca_tool(domain = "namespace.doc.root", verb = "delete")]
async fn remove_doc_root(
    args: RemoveDocRootArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DocRootMutationResult> {
    let conn = db::open_default()?;
    let changed = db::docs::remove_root(&conn, &args.name)?;
    Ok(DocRootMutationResult {
        name: args.name,
        changed,
    })
}

/// List directory names excluded from all doc roots (e.g. node_modules, .git).
#[cfg(feature = "native")]
#[orca_tool(domain = "namespace.doc.pattern", verb = "list")]
async fn list_doc_ignore_patterns(
    _args: ListDocIgnorePatternsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListDocIgnorePatternsOutput> {
    let conn = db::open_default()?;
    let patterns = db::docs::list_ignore_patterns(&conn)?;
    Ok(ListDocIgnorePatternsOutput { patterns })
}

/// [MUTATES STATE] Add a directory name to the global doc ignore list.
#[cfg(feature = "native")]
#[orca_tool(domain = "namespace.doc.pattern", verb = "create")]
async fn add_doc_ignore_pattern(
    args: DocIgnorePatternArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DocIgnorePatternMutationResult> {
    let conn = db::open_default()?;
    let changed = db::docs::add_ignore_pattern(&conn, &args.pattern)?;
    Ok(DocIgnorePatternMutationResult {
        pattern: args.pattern,
        changed,
    })
}

/// [MUTATES STATE] Remove a directory name from the global doc ignore list.
#[cfg(feature = "native")]
#[orca_tool(domain = "namespace.doc.pattern", verb = "delete")]
async fn remove_doc_ignore_pattern(
    args: DocIgnorePatternArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DocIgnorePatternMutationResult> {
    let conn = db::open_default()?;
    let changed = db::docs::remove_ignore_pattern(&conn, &args.pattern)?;
    Ok(DocIgnorePatternMutationResult {
        pattern: args.pattern,
        changed,
    })
}
