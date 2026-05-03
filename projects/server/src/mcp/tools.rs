use serde_json::{Value, json};

pub fn tool_defs() -> Value {
    json!([
        {
            "name": "list_agents",
            "description": "List all available brain agents with their names and descriptions.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_agent",
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
            "name": "run_agent",
            "description": "Delegate a task to a brain agent running on the local LLM (LM Studio). ONLY use this when the task genuinely requires language model reasoning — summarization, explanation, drafting, inference. Do NOT use for: file reads (use read_doc), searches (use search_docs), log lookup (use orca_search_logs), service state (use orca_list_services), schema queries (use get_graphql_info / get_rebuy_spec), or any operation with a deterministic tool that already handles it. Deterministic tools are always preferred over LLM calls. Falls back to Claude Haiku if LM Studio is unreachable.",
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
            "name": "search_logs",
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
            "name": "get_config",
            "description": "Read a Brain configuration/reference document by name (e.g. TOOL_RULES, DELEGATION, SEVERITY_RUBRIC, CANONICAL_SOURCES, CODING_RULES). Call with no name to list available files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Config file name without extension (e.g. TOOL_RULES). Omit to list all available files."
                    }
                }
            }
        },
        {
            "name": "get_context",
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
            "name": "list_services",
            "description": "List all running docker compose services across all rebuy projects. Returns project name, path, and per-service state/health/ports.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_service_logs",
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
            "name": "run_tests",
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
            "name": "list_mcp_servers",
            "description": "List all MCP servers registered in brain.db (brain's own managed registry). Does not include ~/.claude.json servers managed by Claude Code directly.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "add_mcp_server",
            "description": "Add or update an MCP server in brain.db. Use this when the user wants to register a new MCP server for brain to federate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name":    { "type": "string", "description": "Server name (e.g. rebuy-cli)" },
                    "command": { "type": "string", "description": "Executable command (e.g. node)" },
                    "args":    { "type": "array", "items": { "type": "string" }, "description": "Arguments" },
                    "env":     { "type": "object", "description": "Environment variables as key/value pairs" }
                },
                "required": ["name", "command"]
            }
        },
        {
            "name": "remove_mcp_server",
            "description": "Remove an MCP server from brain.db by name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Server name to remove" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "list_schemas",
            "description": "List all MySQL/MariaDB schema databases registered in brain.db.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "add_schema",
            "description": "Add or update a schema database in brain.db. Use container OR host/port, not both.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name":        { "type": "string", "description": "Display name" },
                    "database":    { "type": "string", "description": "Database name" },
                    "user":        { "type": "string", "description": "MySQL user" },
                    "password":    { "type": "string", "description": "MySQL password" },
                    "container":   { "type": "string", "description": "Docker container name (for docker exec connection)" },
                    "host":        { "type": "string", "description": "Host for direct TCP connection" },
                    "port":        { "type": "integer", "description": "Port (default 3306)" },
                    "domainsFile": { "type": "string", "description": "Path to JSON domains file" }
                },
                "required": ["name", "database", "user", "password"]
            }
        },
        {
            "name": "remove_schema",
            "description": "Remove a schema database from brain.db by name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Database name to remove" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "list_docker_runtimes",
            "description": "List all Docker runtimes registered in brain.db. The first enabled runtime is used as the active DOCKER_HOST for MCP subprocesses.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "add_docker_runtime",
            "description": "Add or update a Docker runtime in brain.db. Use socketPath for local unix socket (Colima, Docker Desktop), host for remote TCP daemon, or url for web-based orchestrators (Dockge, Portainer). Multiple runtimes can be registered; the first enabled socket/host runtime is injected as DOCKER_HOST for MCP subprocesses.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name":       { "type": "string", "description": "Runtime name (e.g. colima, docker-desktop, dockge, portainer)" },
                    "socketPath": { "type": "string", "description": "Unix socket path (e.g. ~/.colima/default/docker.sock)" },
                    "host":       { "type": "string", "description": "DOCKER_HOST URL for TCP (e.g. tcp://remote:2376)" },
                    "url":        { "type": "string", "description": "HTTP URL for web-based orchestrators (e.g. https://dockge.internal)" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "remove_docker_runtime",
            "description": "Remove a Docker runtime from brain.db by name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Runtime name to remove" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "register_spec",
            "description": "Fetch an OpenAPI JSON spec from a URL and store it in brain.db. Use this to register an external API spec for use with get_rebuy_spec.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Display name for the spec (e.g. rebuy-cli)" },
                    "url":  { "type": "string", "description": "URL to fetch the OpenAPI JSON from" }
                },
                "required": ["name", "url"]
            }
        },
        {
            "name": "refresh_spec",
            "description": "Re-fetch one or all URL-registered specs from their source URLs and update brain.db.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Spec name to refresh. Omit with all=true to refresh all." },
                    "all":  { "type": "boolean", "description": "Set true to refresh all URL-registered specs" }
                }
            }
        },
        {
            "name": "unregister_spec",
            "description": "Remove a URL-registered spec from brain.db by name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Spec name to remove" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "map_tool",
            "description": "Map an orca tool name to an equivalent tool on a registered external MCP server. Use this to wire orca tool calls through to a specific MCP server's tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name":          { "type": "string", "description": "Registered MCP server name (e.g. rebuy-cli)" },
                    "orca_tool":     { "type": "string", "description": "Orca tool name callers will use" },
                    "external_tool": { "type": "string", "description": "Actual tool name on the external MCP server" }
                },
                "required": ["name", "orca_tool", "external_tool"]
            }
        },
        {
            "name": "unmap_tool",
            "description": "Remove a tool mapping by orca tool name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "orca_tool": { "type": "string", "description": "Orca tool name to unmap" }
                },
                "required": ["orca_tool"]
            }
        },
        {
            "name": "sync_tools",
            "description": "Auto-discover tool mappings for a registered MCP server by interrogating its tools/list endpoint.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name":      { "type": "string", "description": "Server name to sync (omit with all=true to sync all)" },
                    "all":       { "type": "boolean", "description": "Set true to sync all registered servers" },
                    "threshold": { "type": "number", "description": "Confidence threshold for accepting matches (0.0–1.0, default 0.8)" }
                }
            }
        },
        {
            "name": "list_tool_mappings",
            "description": "List all tool mappings, optionally filtered by MCP server name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Server name to filter by (omit for all)" }
                }
            }
        },
        {
            "name": "resolve_library",
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
            "name": "get_library_docs",
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
