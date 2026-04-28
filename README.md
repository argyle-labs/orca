# brain

Context-first AI agent orchestrator. Local-first, with Claude escalation. Ships as a single self-contained binary with the web UI embedded.

## Setup

```sh
make init     # install rust, node, cargo-watch, cargo-audit; npm install
make install  # build site + release binary, install to ~/.local/bin/brain
```

## Usage

```sh
brain                    # interactive session (auto-detects project from cwd)
brain halvor             # interactive session with project context
brain run -a fox "why is this failing?"   # one-shot agent delegation
brain escalate "security question" --project halvor  # ask Claude directly
brain serve              # start web UI on :12000
brain serve --dev        # start API only on :12000 (Vite on :12001 via make dev)
brain mcp-serve          # start MCP stdio server (register with Claude Code)
```

### Register as MCP server

```sh
claude mcp add brain-local -- brain mcp-serve
```

## Commands

| Command | Description |
|---------|-------------|
| `brain` | Start interactive session (local model) |
| `brain <project>` | Start session with project context loaded |
| `brain run -a <agent> "<prompt>"` | One-shot: delegate to a specialist agent |
| `brain escalate "<question>"` | Ask Claude directly (requires API key) |
| `brain serve` | Start web server (embeds site, serves on :12000) |
| `brain serve --dev` | Dev mode: API only, expects Vite on :12001 |
| `brain mcp-serve` | MCP stdio server — exposes brain tools to Claude Code |
| `brain agents` | List available agents |
| `brain projects` | List projects (memory dirs in brain vault) |
| `brain log search "<query>"` | Search all session logs |
| `brain log sessions` | List recent sessions |
| `brain log recall <id>` | Replay a session |
| `brain doctor` | Validate agents, config, symlinks |
| `brain login` | Store Anthropic API key in macOS Keychain |
| `brain auth` | Check API key and LM Studio connectivity |
| `brain audit [path]` | Run Bear audit on a project |

## Interactive commands

| Command | Description |
|---------|-------------|
| `/model` | List models / interactive picker |
| `/model <spec>` | Switch model (e.g. `claude-sonnet-4-6`, `lmstudio:qwen3`) |
| `/flag [note]` | Mark last message as important |
| `/search <query>` | Search session logs |
| `/sessions` | List recent sessions |
| `/recall <id>` | Replay a session |
| `/escalate <question>` | Send to Claude, inject answer into context |
| `/context` | Show messages, tokens, context window % |
| `/tokens` | Token ledger |
| `/narration` | Toggle Brain/Pinky narration |
| `/cleanup` | Kill orphaned brain processes |
| `clear` | Clear conversation history |
| `exit` | Quit |

## Architecture

```
src/
  main.rs        CLI entry, subcommands, project detection
  session.rs     Interactive REPL, chat loop, agent delegation
  config.rs      Config loading (brain vault, API keys, LM Studio URL)
  context.rs     Project context resolution + system prompt assembly
  agents.rs      Agent loading (filesystem + build-time embedded fallback)
  log.rs         Session logging (JSONL), search, recall
  ledger.rs      Token usage tracking
  types.rs       Message, ToolCall, ToolResult, BackendResponse
  auth.rs        macOS Keychain API key storage
  jobs.rs        Background job queue
  tui.rs         Split-pane TUI (ratatui) — default mode, use --classic for readline
  scanner/       File scanner utilities
  mcp.rs         MCP stdio server — JSON-RPC 2.0 over stdin/stdout
  backend/
    mod.rs       ModelBackend trait
    lmstudio.rs  LM Studio OpenAI-compatible backend (default)
    claude.rs    Anthropic Messages API backend (escalation)
  serve/
    mod.rs       HTTP server (axum) — embeds site/dist at compile time via rust-embed
    api.rs       REST API handlers (docs, docker, MCP proxy, schema, logs)
    tree.rs      Vault filesystem tree + search (brain and rebuy roots)
    mcp_client.rs  HTTP → MCP stdio proxy (connects to MCP servers on demand)
    openapi.rs   OpenAPI spec generation (utoipa)
  tools/
    mod.rs       ToolRegistry + definitions (read, write, edit, glob, grep, bash, confirm, delegate)
    bash.rs      Bash execution with permission prompts + timeout
    fs.rs        File read/write/edit
    search.rs    Glob + grep
build.rs         Embeds agent .md files into the binary at compile time
```

## Web server

`brain serve` starts an HTTP server on `:12000`. In release builds the React site is embedded directly in the binary via `rust-embed` — no `site/` directory needed at runtime.

| Endpoint | Description |
|----------|-------------|
| `GET /api/tree` | Filesystem tree for brain or rebuy vault |
| `GET /api/search` | Full-text search across vault docs |
| `GET /api/doc` | Read a vault document by root + path |
| `GET /api/specs` | List registered OpenAPI specs |
| `GET /api/specs/:repo` | Get spec JSON for a repo |
| `GET /api/mcp/tools` | List tools from connected MCP servers |
| `POST /api/mcp/run` | Invoke an MCP tool |
| `GET /api/docker/services` | List docker compose services |
| `POST /api/docker/action` | Start/stop/restart a service |
| `GET /api/schema` | MySQL schema for configured database |
| `GET /api/logs` | Fetch docker compose logs |
| `GET /api/openapi.json` | OpenAPI spec for the brain API |

## MCP server

`brain mcp-serve` runs a JSON-RPC 2.0 server over stdio. Register it with Claude Code and the tools become available as `mcp__brain-local__*`.

| Tool | Description |
|------|-------------|
| `brain_agents` | List all available agents |
| `brain_run` | Delegate a task to a local brain agent |
| `brain_get_context` | Load project memory (MEMORY.md + all memory files) |
| `brain_search_logs` | Search session history by keyword |
| `brain_list_services` | List all running docker compose services |
| `brain_service_logs` | Fetch logs for a running service |
| `list_roots` | List available doc roots (brain, rebuy) |
| `get_tree` | Get doc tree for a root, optionally scoped to a subpath |
| `read_doc` | Read a doc file by root + path |
| `search_docs` | Full-text search across doc roots |
| `list_commands` | List all Claude slash commands and skills |

## Agents

Loaded from `~/brain/ai/claude/agents/`. Wolf is the orchestrator and delegates to specialists via the `delegate` tool.

| Agent | Role |
|-------|------|
| wolf | Orchestrator — routes to the right agent |
| owl | Read and explain code |
| fox | Debug — trace root cause |
| crow | Write code |
| spider | Simplify, find abstractions |
| bear | Critical review + system audit |
| ferret | Code standards enforcement |
| elephant | External docs (TS, React, Next.js, etc.) |
| hawk | Inspect running containers |
| mole | Inspect machine processes and ports |
| badger | Halvor homelab operations |
| boar | BOD dev environment (carl CLI) |
| raven | Take notes, write to brain vault |
| pinky | Session scribe — logs, search, recall |
| lynx | Task planner — minimal agent chain |
| magpie | Scope graduation — promote project rules to global |
| osprey | Escalation judge — local vs Claude |
| ibis | Documentation consistency |
| wren | Agent file maintenance — self-repair |
| bloodhound | Filesystem index + write-through glob cache |
| kestrel | Coverage auditor — finds automation gaps |
| jackdaw | Placement auditor — detects misplaced files/config |
| hound | Privacy sweep — PII, API keys, secrets |
| viper | Security audit |
| shrew | QA and testing |
| otter | Integration and contract validation |
| falcon | DevOps and infrastructure |
| heron | PR review comment formatter |
| mongoose | Adversarial plan reviewer |
| swift | Accessibility auditor |

## Model policy

Local models (LM Studio) run everything by default. Claude is escalation-only, gated by Osprey. Use `brain login` to store an API key for escalation.

## Brain vault

The brain vault at `~/brain` (symlinked from `~/dotfiles/obsidian/`) stores:
- Agent definitions: `ai/claude/agents/*.md`
- Session logs: `ai/claude/logs/sessions/*.jsonl`
- Project memory: `ai/claude/memory/<project>/`
- Slash commands: `ai/claude/commands/`

All vault content is accessible via the web server and MCP tools.

## Make targets

| Target | Description |
|--------|-------------|
| `make init` | Install all required tools (rustup, nvm, cargo-watch, cargo-audit, npm deps) |
| `make build` | Build site + release binary with embedded assets |
| `make install` | Build and install binary to `~/.local/bin/brain` |
| `make dev` | Hot-reload dev mode (cargo-watch + Vite HMR) |
| `make check` | Compile check only |
| `make clean` | Remove target/, site/dist/, site/node_modules/ |
| `make audit` | npm audit fix + cargo audit |
| `make lint` | eslint + cargo clippy |
| `make format` | prettier + cargo fmt |
| `make test` | vitest + cargo test |

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `ANTHROPIC_API_KEY` | (keychain) | API key (env var overrides keychain) |
| `LMSTUDIO_URL` | `http://localhost:1234` | LM Studio server URL |
| `BRAIN_LOG` | (off) | Tracing filter (e.g. `debug`) |
| `BRAIN_AGENTS_DIR` | `~/brain/ai/claude/agents` | Override agents dir at build time |
| `REBUY_ROOT` | `~/code/rebuy` | Override rebuy vault root |
