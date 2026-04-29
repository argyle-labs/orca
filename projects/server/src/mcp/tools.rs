use serde_json::{Value, json};

pub fn tool_defs() -> Value {
    json!([
        {
            "name": "brain_agents",
            "description": "List all available brain agents with their names and descriptions.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "brain_get_agent",
            "description": "Return the full system prompt for a named brain agent. Use this to invoke an agent programmatically via Agent(general-purpose, prompt=<result>+task).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Agent name (e.g. owl, fox, crow, bear)"
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "brain_run",
            "description": "Delegate a task to a local brain agent running on the local LLM. Use for tasks that don't need Claude-level reasoning — code explanation, note-taking, file ops, quick lookups. Returns the agent's full response.",
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
            "name": "brain_search_logs",
            "description": "Search brain session history for a keyword. Returns matching log entries with session ID, role, and content preview.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword to search for across all session logs"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "brain_get_context",
            "description": "Load the memory context for a brain project. Returns MEMORY.md index and all memory files for the project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project name (e.g. halvor, rebuy-db, dotfiles)"
                    }
                },
                "required": ["project"]
            }
        },
        {
            "name": "list_roots",
            "description": "List available documentation roots (rebuy, brain) with file counts and paths.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_tree",
            "description": "Get the compacted documentation tree for a root, optionally scoped to a subpath. Returns a JSON tree of .md files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Root name: rebuy | brain" },
                    "path": { "type": "string", "description": "Optional subpath within root (e.g. \"admin-api\" or \"ai/claude/agents\")" }
                },
                "required": ["root"]
            }
        },
        {
            "name": "read_doc",
            "description": "Read a documentation file by root and relative path (e.g. root=rebuy, path=admin-api/README).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Root name: rebuy | brain | docs" },
                    "path": { "type": "string", "description": "Path relative to root, without extension" },
                    "format": { "type": "string", "description": "Pass \"llm\" when this content will be consumed by a language model — strips decorative markdown (bold, italic, images, horizontal rules) and collapses whitespace to reduce token usage while preserving semantic structure" }
                },
                "required": ["root", "path"]
            }
        },
        {
            "name": "search_docs",
            "description": "Search documentation files for a keyword across one or all roots.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search term (case-insensitive)" },
                    "root": { "type": "string", "description": "Limit to root: rebuy | brain | docs | all (default: all)" },
                    "format": { "type": "string", "description": "Pass \"llm\" when results will be consumed by a language model — strips decorative markdown from matched lines to reduce token usage" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "list_commands",
            "description": "List all Claude slash commands and skills from the brain vault.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "brain_list_services",
            "description": "List all running docker compose services across all rebuy projects. Returns project name, path, and per-service state/health/ports.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "brain_service_logs",
            "description": "Fetch docker compose logs for a running rebuy service. Specify the project path and service name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Absolute path to the project directory (e.g. /Users/scottkey/code/rebuy/admin-api)"
                    },
                    "service": {
                        "type": "string",
                        "description": "Service name as defined in docker-compose (e.g. php, nginx, admin-api-nginx)"
                    },
                    "tail": {
                        "type": "integer",
                        "description": "Number of log lines to return (default: 200)"
                    }
                },
                "required": ["project", "service"]
            }
        },
        {
            "name": "brain_run_tests",
            "description": "Run the brain project test suite. Returns test output with pass/fail counts. Suites: rust (cargo test), frontend (vitest), e2e (playwright), all.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "suite": {
                        "type": "string",
                        "description": "Which suite to run: rust | frontend | e2e | all (default: rust)"
                    }
                }
            }
        },
        {
            "name": "list_rebuy_specs",
            "description": "List all registered OpenAPI specs for rebuy repos. Returns repo name, description, path count, and whether a public or GraphQL schema is available.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_rebuy_spec",
            "description": "Read the full OpenAPI spec for a rebuy repo (e.g. admin-api, apiv2). Returns the complete JSON spec.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Repo name (e.g. admin-api, apiv2, rebuyengine)" }
                },
                "required": ["repo"]
            }
        },
        {
            "name": "get_rebuy_spec_public",
            "description": "Read the public-only OpenAPI spec for a rebuy repo. Contains only publicly documented endpoints.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Repo name (e.g. admin-api, apiv2)" }
                },
                "required": ["repo"]
            }
        },
        {
            "name": "get_rebuy_graphql_schema",
            "description": "Read the raw GraphQL SDL schema for a rebuy repo. Returns the full SDL text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Repo name (e.g. admin-api)" }
                },
                "required": ["repo"]
            }
        },
        {
            "name": "get_graphql_info",
            "description": "Parse and return structured GraphQL schema info for a rebuy repo: queries, mutations, subscriptions, types, inputs, and enums — each with field names, types, and descriptions. Use this instead of get_rebuy_graphql_schema when you need to reason about the schema rather than read the raw SDL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Repo name (e.g. admin-api)" }
                },
                "required": ["repo"]
            }
        },
        {
            "name": "resolve-library-id",
            "description": "Resolve a library name to its context7-compatible library ID. Call this before get-library-docs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "libraryName": { "type": "string", "description": "Library name to look up (npm package, crate, etc.)" }
                },
                "required": ["libraryName"]
            }
        },
        {
            "name": "get-library-docs",
            "description": "Fetch documentation for a library using its context7 library ID. Returns focused, version-accurate docs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "context7CompatibleLibraryID": { "type": "string", "description": "Library ID from resolve-library-id" },
                    "topic": { "type": "string", "description": "Focus on a specific topic or function (optional)" },
                    "tokens": { "type": "integer", "description": "Max tokens to return (default: 8000)" }
                },
                "required": ["context7CompatibleLibraryID"]
            }
        }
    ])
}
