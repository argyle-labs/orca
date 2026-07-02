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

Orca also acts as an MCP **federation hub**: it discovers tools from other registered MCP servers (homelab plugins, third-party servers, etc.) and proxies them. From Claude Code's perspective, all tools from all servers appear as if they come from `orca-local`.

The federation is in `mcp/mod.rs`:

```rust
// projects/server/src/mcp/mod.rs:83
let external = pool.all_tools_filtered(FEDERATION_SKIP).await;

tool_registry.clear();
for tool in &external {
    let name = tool["name"].as_str().unwrap_or("");
    let server = tool["server"].as_str().unwrap_or("");
    // ...
    tool_registry.insert(name.to_string(), (server.to_string(), alias.to_string()));
}
```

`tool_registry` maps external tool names to their owning server. When `tools/call` comes in, the registry is checked first; if the tool is there, the call is forwarded; if not, orca handles it locally.

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

**Why this design:** By keeping agent definitions as text files, they can be:
- Edited without recompiling
- Versioned in git
- Overridden at runtime by dropping a file in `~/.orca/agents/` (filesystem-first lookup)
- Embedded in the binary as fallback

The `load_agent_prompt` function in `projects/agents/src/lib.rs` implements this priority:

```rust
// projects/agents/src/lib.rs:14
pub fn load_agent_prompt(name: &str, agents_dir: &Path) -> Option<String> {
    let path = agents_dir.join(format!("{name}.md"));
    if path.exists() && let Ok(raw) = std::fs::read_to_string(&path) {
        return Some(strip_frontmatter(&raw));
    }
    embedded_agent(name).map(strip_frontmatter)
}
```

1. Check `agents_dir` (usually `~/.orca/agents/`) — filesystem wins
2. Fall back to the embedded copy baked into the binary

This means: during development, editing `~/.orca/agents/wolf.md` changes Wolf's behavior immediately without rebuilding.

**Delegation**: Agents can delegate to other agents by addressing them with `@name`. The session loop handles this — when Wolf says "delegate to @bear", the session loads bear's prompt and re-enters the model loop with that context.

---

## Model Backends: Local vs Cloud

Orca supports two backends:

**LM Studio** (`LMStudioBackend`) — a local OpenAI-compatible server running on your machine. Used for general orchestration tasks. Low latency, no API costs, but limited capability. Communicates via `http://localhost:1234` by default.

**Claude** (`ClaudeBackend`) — Anthropic's API. Used for "escalation" — tasks that require more capability than the local model can handle. The `orca escalate` command and `orca run` route directly to Claude.

The `Model` enum in config:

```rust
// orca_utils/src/config.rs (approximately)
pub enum Model {
    Claude(String),    // model ID like "claude-sonnet-4-6"
    LMStudio(String),  // model ID like "llama-3.2-3b"
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

When the web server handles a request that in turn calls out to external MCP servers, it passes a correlation ID through the chain. The middleware in `serve/middleware.rs` generates a UUID for each request and injects it as `Extension(CorrelationId(uuid))`.

Handlers that call MCP tools pass the ID through:

```rust
// projects/server/src/serve/api/health.rs:70
let result = client.call_tool(&tool, json!({}), &cid).await;
```

This lets you trace a request through logs: the browser request, the MCP proxy call, and the response all share the same ID.
