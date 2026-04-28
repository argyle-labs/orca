# Architecture

brain is a single Rust binary that serves four roles simultaneously:

1. **CLI** — interactive REPL for local AI sessions (`brain`, `brain <project>`)
2. **TUI** — split-pane terminal UI via ratatui (default mode; `--classic` for readline)
3. **Web server** — React site + REST API on `:12000` (`brain serve`)
4. **MCP server** — JSON-RPC 2.0 over stdio for Claude Code integration (`brain mcp-serve`)

## Why one binary

Deployment is `cp brain ~/.local/bin/brain`. No Docker, no node runtime at the install target, no separate web server process. The React site is compiled into the binary at build time via `rust-embed`. See [single-binary.md](single-binary.md).

## Module map

```
src/
  main.rs        CLI entry (clap), subcommand dispatch
  session.rs     Chat loop — sends messages, receives streaming tokens, handles tool calls
  config.rs      Loads brain.toml from ~/brain/config/; resolves all paths
  context.rs     Assembles the system prompt from project memory + agent definitions
  agents.rs      Loads agent .md files from ~/brain/ai/claude/agents/ (+ build-time fallback)
  log.rs         JSONL session logging, search, recall
  ledger.rs      Token accounting per session
  types.rs       Shared types: Message, ToolCall, ToolResult, BackendResponse
  auth.rs        macOS Keychain — stores/retrieves the Anthropic API key
  docs.rs        Embedded repo docs (this docs/ directory, compiled in at build time)
  jobs.rs        Background job queue for async side-effects
  tui.rs         ratatui split-pane UI
  scanner/       File scanner utilities used by tools
  mcp.rs         MCP stdio server — JSON-RPC dispatch + tool implementations
  backend/
    mod.rs       ModelBackend trait (stream_response)
    lmstudio.rs  LM Studio via OpenAI-compatible API (default)
    claude.rs    Anthropic Messages API (escalation only)
  serve/
    mod.rs       axum router; embeds site/dist via rust-embed
    api.rs       All HTTP handlers
    tree.rs      Vault filesystem tree + full-text search
    mcp_client.rs  Spawns MCP server processes and proxies calls over stdio
    openapi.rs   utoipa spec assembly
  tools/
    mod.rs       ToolRegistry — registers tool definitions and routes calls
    bash.rs      Shell execution with interactive permission prompt
    fs.rs        read_file, write_file, edit_file
    search.rs    glob, grep
build.rs         Compiles agent .md files into the binary as a fallback
```

## Request flow (web)

```
browser → axum router → handler → filesystem / MCP client / docker CLI
                      ↓ (root="docs")
                      rust-embed (compiled-in docs/)
```

## Request flow (MCP)

```
Claude Code → stdin → mcp.rs dispatcher → tool implementation
                                         ↓ (brain_run)
                                         session.rs → lmstudio / claude backend
```

## Configuration

All runtime config lives in `~/brain/config/brain.toml` (the vault, not this repo). The binary reads it at startup. There is no config bundled into the binary — this keeps developer configuration out of the shipped artifact.
