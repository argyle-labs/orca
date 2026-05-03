# orca-plugin.toml schema

Plugins declare their identity, MCP server, and which universal commands they implement.

## Full schema

```toml
[plugin]
id          = "example"           # unique slug, used in DB and routing
version     = "1.0.0"             # semver, informational
tier        = "external"          # internal | external | personal
context_injection = "full"        # full | minimal | none — how much context orca injects

# MCP server that backs this plugin
[plugin.mcp]
command = "node"
args    = ["/path/to/server/build/index.js"]
env     = { SOME_VAR = "value" }  # optional

# Universal command mapping: exposed_name = "internal_mcp_tool_name"
# Keys become the tool names exposed through orca's federation surface.
# Values are the tool names the plugin's MCP server uses internally.
# Unmapped tools from the MCP server are not exposed.
[plugin.commands]
search_docs       = "example_docs_search"
list_docs         = "example_docs_list"
read_doc          = "example_docs_read"
get_status        = "example_status"
```

## Rules

- **Keys** (left side) are the universal command names — no app prefix, pure verbs.
- **Values** (right side) are the plugin's internal MCP tool names.
- Tools not listed in `[plugin.commands]` are not federated.
- If two plugins map the same universal command, orca uses the first connected plugin.
  Resolution order follows plugin registration order in the DB.
- Orca's own native tools take precedence over any plugin mapping with the same name.

## Tiers

| tier       | meaning                                          |
|------------|--------------------------------------------------|
| internal   | orca itself / core system tools                  |
| external   | third-party or proprietary tools (rebuy, ctx7)   |
| personal   | user-specific, local-only                        |

## Context injection

| value   | meaning                                                        |
|---------|----------------------------------------------------------------|
| full    | orca injects full project memory + plugin context on session start |
| minimal | only inject when explicitly referenced                         |
| none    | no automatic context injection                                 |

## Example: rebuy plugin

```toml
[plugin]
id                = "rebuy"
version           = "0.1.0"
tier              = "external"
context_injection = "full"

[plugin.mcp]
command = "node"
args    = ["/Users/scottkey/code/rebuy/rebuy-cli-mcp-server/build/index.js"]

[plugin.commands]
# Knowledge
search_docs        = "rebuy_docs_search"
list_docs          = "rebuy_docs_list"
read_doc           = "rebuy_docs_read"
search_spec        = "rebuy_spec_search"
list_specs         = "rebuy_spec_list"
get_spec_endpoint  = "rebuy_spec_endpoint"
get_spec_schema    = "rebuy_spec_schema"
search_graphql     = "rebuy_graphql_search"
list_graphql       = "rebuy_graphql_list"
get_graphql_op     = "rebuy_graphql_operation"

# Status / health
get_status         = "rebuy_status"
check_health       = "rebuy_doctor"
get_version        = "rebuy_version"

# Environment lifecycle
start_env          = "rebuy_env_start"
stop_env           = "rebuy_env_stop"
restart_env        = "rebuy_env_restart"
get_env_status     = "rebuy_env_status"
switch_env         = "rebuy_env_switch"
```

## Example: context7 plugin

```toml
[plugin]
id                = "context7"
version           = "1.0.0"
tier              = "external"
context_injection = "minimal"

[plugin.mcp]
command = "npx"
args    = ["-y", "@upstash/context7-mcp"]

[plugin.commands]
resolve_library  = "resolve-library-id"
get_library_docs = "get-library-docs"
```
