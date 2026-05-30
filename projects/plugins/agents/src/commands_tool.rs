//! Docs domain — last surviving tool is `namespace.doc.list-commands`
//! (slash-command listing). The filesystem-shaped tools
//! (`namespace.doc.{list-roots,tree,full-tree,read,search}`) dissolved
//! into the generic `fs.*` surface in slice 2 of crate-topology-v2 —
//! see [[project_fs_crate]].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsOutput {
    pub commands: Vec<String>,
}

/// List all Claude slash commands and skills embedded in the orca binary.
#[orca_tool(domain = "namespace.doc", verb = "list-commands")]
async fn list_commands(
    _args: ListCommandsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListCommandsOutput> {
    Ok(ListCommandsOutput {
        commands: crate::commands::list_embedded_commands(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::ToolCtx;
    use contract::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn empty_ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-list-commands-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn list_commands_returns_embedded_set() {
        let ctx = empty_ctx();
        let out = list_commands(ListCommandsArgs {}, &ctx).await.unwrap();
        assert_eq!(out.commands, crate::commands::list_embedded_commands());
    }
}
