// Static tool definitions for tools not yet converted to OrcaTool.
// TODO: convert run_agent, then delete this file entirely.
use serde_json::{Value, json};

pub fn tool_defs() -> Value {
    json!([
        {
            "name": "run_agent",
            "description": "Delegate a task to an orca agent. The backend (LM Studio, server-side Anthropic, or delegation back to Claude Code) is selected by agent_backend_status — see the agent_backend_* tools to configure. ONLY use this when the task genuinely requires language model reasoning — summarization, explanation, drafting, inference. Do NOT use for: file reads (use read_doc), searches (use search_docs), log lookup (use search_logs), service state (use list_services), schema queries (use get_graphql_info / get_rebuy_spec), or any operation with a deterministic tool that already handles it. Deterministic tools are always preferred over LLM calls. Hard-fail: no silent fallback between backends. When the resolver picks Claude and server-side Anthropic is disabled, run_agent returns a JSON envelope { action: 'delegate_to_claude_code', ... } and the caller must invoke get_agent + Agent(general-purpose) itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "Agent name (e.g. wolf, owl, fox, crow, raven, badger)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Task or question to send to the agent"
                    }
                },
                "required": ["agent", "prompt"]
            }
        },
        {
            "name": "resolve_library",
            "description": "Resolve a library name to its Context7-compatible ID. Call before get_library_docs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "libraryName": { "type": "string" }
                },
                "required": ["libraryName"]
            }
        },
        {
            "name": "get_library_docs",
            "description": "Fetch up-to-date documentation for a library via Context7.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "context7CompatibleLibraryID": { "type": "string" },
                    "topic": { "type": "string" },
                    "tokens": { "type": "integer" }
                },
                "required": ["context7CompatibleLibraryID"]
            }
        }
    ])
}
