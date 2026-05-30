// Tool-use bridging between LLM blobs and typed tools; HashMap/Value are wire-format passthrough.
#![allow(clippy::disallowed_types)]
pub mod bash;

use crate::backend::{OutputSink, stdout_sink};
use anyhow::Result;
use bash::BashPermissions;
use contract::{ToolDef, ToolResult};
use files::ops;
use serde_json::{Value, json};
use utils::search;

pub struct ToolRegistry {
    pub permissions: BashPermissions,
    pub working_dir: Option<String>,
    pub output: OutputSink,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        ToolRegistry {
            permissions: BashPermissions::default(),
            working_dir: None,
            output: stdout_sink(),
        }
    }
}

impl ToolRegistry {
    /// All tool definitions exposed to the model.
    pub fn definitions() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "read_file".into(),
                description: "Read the contents of a file at the given path.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute or relative file path" }
                    },
                    "required": ["path"]
                }),
            },
            ToolDef {
                name: "write_file".into(),
                description: "Write content to a file, creating it if it doesn't exist.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolDef {
                name: "edit_file".into(),
                description: "Replace the first occurrence of old_string with new_string in a file. Fails if old_string is not unique.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string", "description": "Exact text to replace" },
                        "new_string": { "type": "string", "description": "Replacement text" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            ToolDef {
                name: "glob".into(),
                description: "Find files matching a glob pattern. Returns a newline-separated list of paths.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern, e.g. \"**/*.rs\"" },
                        "base": { "type": "string", "description": "Optional base directory to search in" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDef {
                name: "grep".into(),
                description: "Search file contents for a string. Returns matching lines with file:line format.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "String to search for" },
                        "path": { "type": "string", "description": "File or directory to search" },
                        "case_insensitive": { "type": "boolean", "default": false }
                    },
                    "required": ["pattern", "path"]
                }),
            },
            ToolDef {
                name: "bash".into(),
                description: "Execute a bash command. User will be prompted for permission unless previously approved.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to run" }
                    },
                    "required": ["command"]
                }),
            },
            ToolDef {
                name: "confirm".into(),
                description: "Ask the user for confirmation before proceeding. Use this after presenting a plan or before making changes. Returns 'yes' or 'no'.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "What to ask the user, e.g. 'Proceed with the audit?'" }
                    },
                    "required": ["question"]
                }),
            },
            ToolDef {
                name: "delegate".into(),
                description: "Delegate a task to a specialist agent. The agent runs a sub-conversation with full tool access and returns its result. Common generic agents: owl (explain code), fox (debug), crow (write code), spider (simplify), bear (review + audit), ferret (code standards), hawk (containers), mole (processes/ports), elephant (external docs), lynx (plan), raven (notes), otter (session logs). Additional domain-specific agents may be installed by plugins.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "agent": { "type": "string", "description": "Agent name (e.g. fox, crow, owl)" },
                        "task": { "type": "string", "description": "What the agent should do — include all necessary context" }
                    },
                    "required": ["agent", "task"]
                }),
            },
        ]
    }

    /// Dispatch a tool call and return a ToolResult.
    pub async fn execute(&mut self, tool_use_id: String, name: &str, input: &Value) -> ToolResult {
        let result = self.run(name, input).await;
        match result {
            Ok(content) => ToolResult {
                tool_use_id,
                content,
                is_error: false,
            },
            Err(e) => ToolResult {
                tool_use_id,
                content: format!("Error: {e}"),
                is_error: true,
            },
        }
    }

    async fn run(&mut self, name: &str, input: &Value) -> Result<String> {
        match name {
            "read_file" => {
                let path = str_field(input, "path")?;
                ops::read_file(&path)
            }
            "write_file" => {
                let path = str_field(input, "path")?;
                let content = str_field(input, "content")?;
                ops::write_file(&path, &content)
            }
            "edit_file" => {
                let path = str_field(input, "path")?;
                let old = str_field(input, "old_string")?;
                let new = str_field(input, "new_string")?;
                ops::edit_file(&path, &old, &new)
            }
            "glob" => {
                let pattern = str_field(input, "pattern")?;
                let base = input["base"].as_str();
                search::glob_files(&pattern, base)
            }
            "grep" => {
                let pattern = str_field(input, "pattern")?;
                let path = str_field(input, "path")?;
                let ci = input["case_insensitive"].as_bool().unwrap_or(false);
                search::grep_content(&pattern, &path, ci)
            }
            "bash" => {
                let command = str_field(input, "command")?;
                bash::run_bash(
                    &command,
                    &mut self.permissions,
                    self.working_dir.as_deref(),
                    &self.output,
                )
                .await
            }
            _ => anyhow::bail!("unknown tool: {name}"),
        }
    }
}

fn str_field(input: &Value, key: &str) -> Result<String> {
    input[key]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing required field: {key}"))
}
