//! Docs domain — last surviving tool is `namespace.doc.list-commands`
//! (slash-command listing). The filesystem-shaped tools
//! (`namespace.doc.{list-roots,tree,full-tree,read,search}`) dissolved
//! into the generic `fs.*` surface in slice 2 of crate-topology-v2 —
//! see [[project_fs_crate]].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsOutput {
    pub commands: Vec<String>,
}

/// List all Claude slash commands and skills embedded in the orca binary.
#[cfg(feature = "native")]
#[orca_tool(domain = "namespace.doc", verb = "list-commands")]
async fn list_commands(
    _args: ListCommandsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListCommandsOutput> {
    Ok(ListCommandsOutput {
        commands: agents::commands::list_embedded_commands(),
    })
}
