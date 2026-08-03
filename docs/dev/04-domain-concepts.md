# Domain Concepts

Orca has several concepts that are specific to AI orchestration and to orca's own design. Understanding what these things *are* — not just where they live in the code — makes the code make sense.

---

## MCP: The Model Context Protocol

MCP is a protocol for AI assistants to call external tools. An MCP server exposes a set of tools; an AI client (like Claude Code) discovers and calls them.

The protocol is JSON-RPC 2.0 over stdio (or HTTP). The core messages:

| Message | What it does |
|---|---|
| `initialize` | Client says hello, server responds with capabilities and protocol version |
| `tools/list` | Client asks what tools are available; server returns an array of tool definitions |
| `tools/call` | Client calls a tool by name with arguments; server executes and returns the result |
| `ping` | Keep-alive |

Each tool definition has a `name` (what the client calls), a `description` (used by the LLM to decide when to use it), and an `inputSchema` (JSON Schema the LLM follows to construct calls). For orca's own tools, the `#[orca_tool]` annotation generates all three directly from the function and its args struct.

Orca implements an MCP server (`orca mcp-serve`). Claude Code registers it as `orca-local`. It also acts as an MCP **federation hub**: it discovers tools from other registered MCP servers and proxies them, so from the client's perspective every tool appears to come from `orca-local`.

The federation and routing live in `serve()` in [`projects/server/src/mcp/mod.rs`](../../projects/server/src/mcp/mod.rs). An in-memory `tool_registry` maps each federated tool name to its owning server; on `tools/call` the registry is checked first (forward to the owner), and orca's own `#[orca_tool]` tools — the names in `dispatch::names()` — are dispatched locally through `dispatch::dispatch_text`. See [Hot Paths, Flow 1](03-hot-paths.md#flow-1-a-tool-call-from-claude-code) for the full routing order.

---

## Agents: Named System Prompts

In orca's model, an "agent" is a named Markdown file with YAML frontmatter. It defines the persona and capabilities of one AI character. All agents share the same LLM; what differs is the system prompt.

A frontmatter block carries `name`, `description`, `tools`, `model`, and `color`; the body after the frontmatter is the system prompt.

The `wolf`/`otter`/… roster lives in the external `argyle-labs/agents` plugin, which contributes it over the `agents.register` capability at runtime. Any plugin can register its own agents the same way. Core owns the *resolution mechanism* below (and the dev hot-reload path) and resolves each prompt against the registered roster or a user override.

**Why this design:** keeping agent definitions as text means they can be edited without recompiling, versioned in git, overridden at runtime by dropping a file in `~/.orca/agents/`, or shipped inside a plugin.

Resolution is `load_agent_prompt` in [`projects/agents/src/resolve.rs`](../../projects/agents/src/resolve.rs), which delegates to `load_agent_prompt_from_dirs` in [`projects/agents/src/embedded.rs`](../../projects/agents/src/embedded.rs). The priority is: user/override directories (e.g. `~/.orca/agents/`) first, then the embedded copy. Both paths run the raw file through `strip_frontmatter` to yield the prompt body. This is why editing `~/.orca/agents/wolf.md` changes Wolf's behavior immediately, without a rebuild.

**Delegation:** agents can hand off to one another; the session's delegate path (in [`sessions/session/delegate.rs`](../../projects/conversation/src/sessions/session/delegate.rs)) loads the target agent's prompt and re-enters the model loop with that context.

---

## Model Backends: Local vs Cloud

Orca supports three backends, all implementing the `ModelBackend` trait in [`projects/model/src/backend/mod.rs`](../../projects/model/src/backend/mod.rs):

- **LM Studio** (`LMStudioBackend`) — a local OpenAI-compatible server. Low latency, no API cost, limited capability.
- **Ollama** (`OllamaBackend`) — another local/network OpenAI-compatible server.
- **Claude** (`ClaudeBackend`) — Anthropic's API, used for *escalation*: tasks beyond what a local model handles reliably.

Which one a session uses is chosen by the `Model` enum in [`projects/contract/src/config/mod.rs`](../../projects/contract/src/config/mod.rs) — `Claude(String)`, `LMStudio { id, url }`, or `Ollama { id, url }`. `build_backend` (in `backend/mod.rs`) constructs the right client from that enum. Sessions default to a local model (local-first); Claude is escalation-only. The session can switch backends mid-conversation when an agent requests a different model or the orchestrator decides to escalate.

---

## The Vault: Memory at `~/.orca/`

The "vault" is the directory at `~/.orca/` (or wherever `config.vault_dir` points). It is orca's persistent memory — not code, not config, but knowledge about your projects.

Rough structure:
```
~/.orca/
  memory/<project>/MEMORY.md   ← project context injected into the system prompt
  agents/<name>.md             ← override or custom agents
  logs/*.jsonl                 ← session logs
  orca.db                      ← encrypted SQLite runtime registry
```

When you run `orca --project <name>`, `ProjectContext::resolve("<name>", config)` (in [`projects/conversation/src/sessions/context.rs`](../../projects/conversation/src/sessions/context.rs)) loads `~/.orca/memory/<name>/MEMORY.md` and prepends it to the system prompt. The `MEMORY.md` is plain Markdown you maintain by hand — context written for the AI to read, not structured data.

`detect_project_from_cwd` in [`projects/server/src/main.rs`](../../projects/server/src/main.rs) infers the project automatically: it walks a few cwd ancestors and, if one matches a directory under the memory root, loads that project's context without you naming it.

---

## Sessions and Conversation History

A `Session` (in [`projects/conversation/src/sessions/session/mod.rs`](../../projects/conversation/src/sessions/session/mod.rs)) represents one interactive conversation. It holds the loaded `Config`, the resolved `ProjectContext` and system prompt, the `Vec<Message>` history, the active `Box<dyn ModelBackend>`, an `OutputSink`, and a `CancellationToken`.

Each call to `backend.chat()` passes the full history, so the model sees every prior turn. **Tool results are also messages:** when the model calls a tool, the session appends the tool-use request, executes the tool, appends the tool result, and calls `chat()` again — looping until the model ends its turn with a final text response.

**Session logs** are written to `~/.orca/logs/` as JSONL, one JSON object per message (role, content, agent, timestamp, importance flag).

---

## The `OutputSink` Abstraction

`OutputSink` (defined in [`projects/model/src/backend/mod.rs`](../../projects/model/src/backend/mod.rs)) is a shared, thread-safe writer — an `Arc<Mutex<Box<dyn Write + Send>>>` — that unifies "where does model output go":

- **Interactive sessions:** a stdout sink → tokens stream to the terminal.
- **Background jobs (the `agent.run` tool):** a buffer sink → tokens collect in memory and are returned as a string.

Because the backend just writes to a sink, its `chat()` method is identical in both cases; the caller decides whether the user sees tokens live or receives them all at once.

---

## Correlation IDs

When the HTTP server handles a request that in turn calls out to federated MCP servers, it threads a correlation ID through the chain. The `correlation_id` middleware in [`projects/server/src/serve/middleware.rs`](../../projects/server/src/serve/middleware.rs) generates a UUID per request and injects it as an `Extension`; handlers that fan out to MCP tools pass it along. This lets you trace one request — the inbound call, the proxied MCP calls, and the response — by a single shared ID in the logs.
