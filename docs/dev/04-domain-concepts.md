# Domain Concepts

Orca has several concepts that are specific to AI orchestration and to orca's own design. Understanding what these things *are* — not just where they live in the code — makes the code make sense.

---

## MCP: The Model Context Protocol

MCP is a protocol for AI assistants to call external tools. An MCP server exposes a set of tools; an AI client (like Claude Code) discovers and calls them.

The protocol is JSON-RPC 2.0 over stdio (or HTTP). The core messages:

| Message | What it does |
|---|---|
| `initialize` | Client says hello, server responds with capabilities and protocol version |
| `tools/list` | Client asks what tools are available; server returns array of tool definitions |
| `tools/call` | Client calls a tool by name with arguments; server executes and returns result |
| `ping` | Keep-alive |

Each tool definition has:
- `name` — the string the client uses to call it
- `description` — used by the LLM to decide when to use the tool
- `inputSchema` — JSON Schema for the arguments; the LLM follows this to construct calls

Orca implements an MCP server (`orca mcp-serve`). Claude Code registers orca as `orca-local` in its MCP config. Every time Claude Code needs information about your projects, it calls orca tools.

Orca also acts as an MCP **federation hub**: federation (talking to other registered MCP servers and proxying their tools) is the external `mcp` plugin's job, not core's. The plugin's `mcp.*` tools reach the stdio server via the plugin-tool bridge; core's `mcp/mod.rs` only implements the MCP protocol itself.

In `tools/call`, plugin-declared tools are recognized by their dotted `<plugin_id>.<tool>` names and forwarded to the daemon's `PluginRegistry` (`projects/server/src/mcp/mod.rs:167`); everything else dispatches through the local `#[orca_tool]` inventory registry. From Claude Code's perspective, all tools — core, plugin, and federated — appear to come from `orca-local`.

---

## Agents: Named System Prompts

In orca's model, an "agent" is a named Markdown file with YAML frontmatter. It defines the persona and capabilities of one AI character. All agents are the same LLM; what differs is the system prompt.

Example frontmatter from `wolf.md`:
```yaml
---
name: wolf
description: Primary orchestrator. Routes every task to the right agent...
tools: Read, Glob, Grep, Bash, Write, Edit, WebFetch, WebSearch, Agent
model: inherit
color: orange
---
```

The body of the file is the system prompt that Wolf uses.

**Why this design:** agent definitions are plain markdown, so they can be versioned in git (in their owning plugin repo), materialized to `~/.claude/agents/` by `orca install`, and swapped by loading a different provider — no recompiling orca core.

Agents are **contributed by plugins**, not baked into the binary. Any plugin registers an `AgentProvider` in the core registry (`projects/contract/src/agents.rs`); the base roster (wolf/otter/…) comes from the external `argyle-labs/agents` plugin. `load_agent_prompt` composes across all registered providers, later registration winning:

```rust
// projects/contract/src/agents.rs:284
pub fn load_agent_prompt(name: &str) -> Option<String> {
    providers()
        .iter()
        .rev()
        .flat_map(|p| p.agents())
        .find(|a| a.name == name)
        .map(|a| a.body)
}
```

With no agents plugin loaded the registry is empty and nothing is materialized — core has no embedded fallback and no hard-coded agent names.

**Delegation**: Agents can delegate to other agents by addressing them with `@name`. The session loop handles this — when Wolf says "delegate to @bear", the session loads bear's prompt and re-enters the model loop with that context.

---

## Model Backends: Local vs Cloud

Orca supports three backends:

**LM Studio** (`LMStudioBackend`) — a local OpenAI-compatible server running on your machine. Low latency, no API costs, but limited capability. `http://localhost:1234` by default.

**Ollama** (`OllamaBackend`) — another OpenAI-compatible local/network server, same role as LM Studio.

**Claude** (`ClaudeBackend`) — Anthropic's API. Used for "escalation" — tasks that require more capability than the local model can handle. The `orca escalate` command routes directly to Claude.

The `Model` enum in config:

```rust
// projects/contract/src/config/mod.rs:105
pub enum Model {
    Claude(String),                    // model ID like "claude-sonnet-4-6"
    LMStudio { id: String, url: String },
    Ollama { id: String, url: String },
}
```

`build_backend()` constructs the right client based on which model is configured. Sessions default to the local model; escalation uses Claude explicitly.

The session can switch backends mid-conversation if the user invokes an agent that requests a different model — or when the orchestrator decides the local model cannot handle a task and escalates.

---

## The Vault: Memory at `~/.orca/`

The "vault" is the directory at `~/.orca/` (or wherever `config.vault_dir` points). It is orca's persistent memory — not code, not config, but knowledge about your projects.

Structure:
```
~/.orca/
  memory/
    meerkat/
      MEMORY.md          ← project-specific context injected into system prompt
    my-project/
      MEMORY.md
    dev/
      MEMORY.md
  agents/
    wolf.md              ← override or custom agents
  logs/
    2025-05-01-*.jsonl   ← session logs
  openapi/               ← registered external OpenAPI specs
  orca.db                ← SQLite: MCP servers, schemas, Docker runtimes, tool mappings
```

When you run `orca meerkat` (project name as argument), `ProjectContext::resolve("meerkat", config)` loads `~/.orca/memory/meerkat/MEMORY.md` and prepends it to the system prompt. Wolf now knows everything in that file about the Meerkat project.

The MEMORY.md is plain Markdown — you write it and update it as the project evolves. It is not structured data; it is context written for the AI to read.

The `detect_project_from_cwd` function in `main.rs` also infers the project automatically:

```rust
// projects/server/src/main.rs:440
fn detect_project_from_cwd(config: &Config) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    for ancestor in cwd.ancestors().take(4) {
        let name = ancestor.file_name()?.to_string_lossy().to_string();
        if config.memory_root.join(&name).exists() {
            return Some(name);
        }
    }
    None
}
```

If your current directory is `~/code/meerkat/` and `~/.orca/memory/meerkat/` exists, orca loads the Meerkat context automatically without you specifying it.

---

## Sessions and Conversation History

A `Session` represents one interactive conversation. It holds:

- `config: Config` — the loaded configuration (paths, API keys, model selection)
- `ctx: ProjectContext` — the resolved project context and system prompt
- `messages: Vec<Message>` — the conversation history (user + assistant + tool result messages)
- `backend: Box<dyn ModelBackend>` — the active model backend
- `output: OutputSink` — where tokens are written (stdout for TUI, buffer for background)
- `cancel: CancellationToken` — allows in-progress model calls to be interrupted

Each call to `backend.chat()` passes the full `messages` history. The model sees every prior turn. When the model responds, its response is appended to `messages`. This is how the model maintains context across turns.

**Tool results** are also messages. When the model calls a tool, the session:
1. Appends the model's tool-use request to `messages`
2. Executes the tool locally
3. Appends the tool result as a special `tool_result` message
4. Calls `backend.chat()` again with the extended history

This continues until the model returns `stop_reason: "end_turn"` with a final text response.

**Session logs** are written to `~/.orca/logs/`. Each session is a JSONL file where each line is a JSON object representing one message (role, content, agent, timestamp, importance flag). The `search_logs` MCP tool queries these.

---

## The `OutputSink` Abstraction

The `OutputSink` type unifies "where does model output go":

```rust
// projects/model/src/backend/mod.rs:27
pub type OutputSink = Arc<Mutex<Box<dyn Write + Send>>>;
```

- **Interactive sessions:** `stdout_sink()` → tokens stream to the terminal
- **Background jobs (MCP `run_agent`):** `buffer_sink()` → tokens collect in memory, returned as a string

This means the model backend's `chat()` method is identical in both cases — it writes to a sink and never knows whether the user sees tokens live or receives them all at once. The session or caller decides.

---

## Correlation IDs

When the web server handles a request, the middleware in `serve/middleware.rs` reads or generates an `x-correlation-id` header per request and injects it as `Extension(CorrelationId(id))` (`projects/server/src/serve/middleware.rs:17`). Every request/response log line carries `correlation_id = %cid`.

This lets you trace a request through logs: the browser request, any downstream calls, and the response all share the same ID.
