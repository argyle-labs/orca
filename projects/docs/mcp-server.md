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

## Available tools

| Tool | What it does |
|------|-------------|
| `brain_agents` | Lists all agents with names + descriptions |
| `brain_run` | Delegates a task to a local brain agent (runs a full session) |
| `brain_get_context` | Returns MEMORY.md + all memory files for a project |
| `brain_search_logs` | Full-text search across all JSONL session logs |
| `brain_list_services` | Lists docker compose services across rebuy projects |
| `brain_service_logs` | Fetches docker compose logs for a service |
| `list_roots` | Lists available doc roots (brain, rebuy, docs) with file counts |
| `get_tree` | Returns the compacted doc tree for a root |
| `read_doc` | Reads a doc file by root + path |
| `search_docs` | Full-text search across doc roots |
| `list_commands` | Lists all Claude slash commands from the vault |

## The `docs` root

`root="docs"` is special — it reads from embedded content compiled into the binary, not from the filesystem. This means the brain architecture docs (this file) are always available via `read_doc` regardless of where the binary is installed.

## brain_run implementation

`brain_run` starts a full `session.rs` session with a `buffer_sink` — the session runs to completion and the entire output is returned as a string. This means the local model processes the task fully before returning. It is not streaming.

The agent name is resolved by prefixing `@<agent>` to the prompt and sending to Wolf (or directly to the named agent if specified). This uses the same delegation path as interactive sessions.

## Protocol notes

- Notifications (JSON-RPC messages without an `id`) are silently dropped — replying would violate the protocol
- `initialize` returns `protocolVersion: "2024-11-05"` which matches Claude Code's expectation
- Tools that fail return `isError: true` with the error message in `content[0].text`
