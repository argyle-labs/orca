# MCP Server

`brain mcp-serve` runs a Model Context Protocol server over stdio. Register it with Claude Code and the tools become available as `mcp__brain-local__*` in any Claude Code session.

## Registration

```sh
claude mcp add brain-local -- brain mcp-serve
```

This tells Claude Code to spawn `brain mcp-serve` as a subprocess whenever it needs brain tools. The process communicates via JSON-RPC 2.0 on stdin/stdout.

## Why stdio transport

MCP supports HTTP+SSE and stdio transports. stdio is used here because:
- No port conflict — doesn't require a running `brain serve` instance
- Claude Code manages the process lifecycle
- Works on any machine without network configuration
- The process is cheap to spawn; it exits when Claude Code disconnects

## Tool federation

brain's own tools are defined in `mcp/tools.rs`. On every `tools/list` request, brain also queries all registered MCP servers from brain.db (`brain mcp list`) and merges their tools into the response. Tools from federated servers are routed directly to the owning subprocess on `tools/call`.

Brain skips `brain-local` and `context7` from federation to prevent recursive loops and conflicts. Any tool name conflict (federated tool has same name as a brain-owned tool) is won by brain.

**To register a federated MCP server:**
```sh
brain mcp add rebuy-cli --command node --args ~/code/rebuy/rebuy-cli/dist/mcp-server.js
```

After registration, `brain mcp-serve` will expose the server's tools in the next `tools/list` response.

## Brain-owned tools

### Agent tools

| Tool | What it does |
|------|-------------|
| `brain_agents` | Lists all agents with names + descriptions |
| `brain_get_agent` | Returns the full system prompt for a named agent |
| `brain_run` | Delegates a task to a local agent (full session, buffered output) |
| `brain_search_logs` | Full-text search across all JSONL session logs |
| `brain_get_config` | Reads a brain reference doc by name (TOOL_RULES, DELEGATION, etc.) |
| `brain_get_context` | Returns MEMORY.md + all memory files for a project |

### Documentation tools

| Tool | What it does |
|------|-------------|
| `list_roots` | Lists available doc roots (brain, rebuy, docs) with file counts |
| `get_tree` | Returns the compacted doc tree for a root, optionally scoped to a subpath |
| `read_doc` | Reads a doc file by root + path (strips decorative markdown with format="llm") |
| `search_docs` | Full-text search across doc roots |
| `list_commands` | Lists all Claude slash commands from the vault |

### Service tools

| Tool | What it does |
|------|-------------|
| `brain_list_services` | Lists all Docker Compose services across all rebuy projects |
| `brain_service_logs` | Fetches Docker Compose logs for a service (tail parameter) |
| `brain_run_tests` | Runs the brain test suite (rust \| frontend \| e2e \| all) |

### OpenAPI spec tools

| Tool | What it does |
|------|-------------|
| `list_rebuy_specs` | Lists all registered specs (disk + URL-registered) |
| `get_rebuy_spec` | Returns the full OpenAPI JSON for a repo (disk first, DB fallback) |
| `get_rebuy_spec_public` | Returns the public-only OpenAPI spec |
| `get_rebuy_graphql_schema` | Returns the raw GraphQL SDL for a repo |
| `get_graphql_info` | Returns parsed GraphQL schema: queries, mutations, types, enums |
| `brain_spec_register` | Fetches OpenAPI JSON from a URL and stores it in brain.db |
| `brain_spec_refresh` | Re-fetches one or all URL-registered specs from their stored URLs |
| `brain_spec_unregister` | Removes a URL-registered spec from brain.db |

### MCP server registry tools

| Tool | What it does |
|------|-------------|
| `brain_mcp_list` | Lists all MCP servers in brain.db |
| `brain_mcp_add` | Adds or updates an MCP server in brain.db |
| `brain_mcp_remove` | Removes an MCP server from brain.db by name |

### Schema database registry tools

| Tool | What it does |
|------|-------------|
| `brain_schema_list` | Lists all schema databases in brain.db |
| `brain_schema_add` | Adds or updates a schema DB (container or host/port) |
| `brain_schema_remove` | Removes a schema DB by name |

### Docker runtime registry tools

| Tool | What it does |
|------|-------------|
| `brain_docker_list` | Lists all Docker runtimes in brain.db |
| `brain_docker_add` | Adds or updates a Docker runtime (socket, host, or url) |
| `brain_docker_remove` | Removes a Docker runtime by name |

### Context7 proxy tools

| Tool | What it does |
|------|-------------|
| `resolve-library-id` | Resolves a library name to its context7 ID |
| `get-library-docs` | Fetches docs for a library using its context7 ID |

## The `docs` root

`root="docs"` is special — it reads from content compiled into the binary, not the filesystem. The brain architecture docs are always available via `read_doc` regardless of where the binary is installed.

## brain_run implementation

`brain_run` starts a full `session.rs` session with a `buffer_sink` — the session runs to completion and the entire output is returned as a string. The local model processes the task fully before returning. Not streaming.

The agent name is resolved by prefixing `@<agent>` to the prompt and sending to Wolf (or directly to the named agent if specified). This uses the same delegation path as interactive sessions.

Falls back to Claude Haiku if LM Studio is unreachable.

## Protocol notes

- Notifications (JSON-RPC messages without an `id`) are silently dropped — replying would violate the protocol
- `initialize` returns `protocolVersion: "2024-11-05"` which matches Claude Code's expectation
- Tools that fail return `isError: true` with the error message in `content[0].text`
- Dead federated connections (MCP server closed) are evicted from the pool automatically — next call reconnects

## Adding a new MCP tool

1. Define the schema in `projects/server/src/mcp/tools.rs` → `tool_defs()` JSON array
2. Implement the handler in `projects/server/src/mcp/handlers.rs` (or the relevant `mcp/*.rs` module)
3. Add the dispatch match arm in `projects/server/src/mcp/mod.rs` → `dispatch()`
4. If the tool also has a REST API surface, add it to `projects/server/src/serve/api/` and register in `openapi.rs`
