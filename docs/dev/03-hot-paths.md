# Hot Paths

Three code flows matter most for day-to-day development. This document traces each one from entry point to response, naming the functions and files along the way. Read it with the source open.

---

## Flow 1: A Tool Call from Claude Code

Claude Code calls an orca tool (e.g. `config.detail`) over the MCP protocol. The whole path lives in `serve()` in [`projects/server/src/mcp/mod.rs`](../../projects/server/src/mcp/mod.rs).

### Step 1: stdin arrives

`orca mcp-serve` runs `mcp::serve()`, which reads newline-delimited JSON-RPC from stdin in a loop. Claude Code writes one request per line to the subprocess.

### Step 2: JSON-RPC parsing

Each line is parsed as untyped JSON (`serde_json::Value`); the `id`, `method`, and `params` are pulled out. Notifications (requests without an `id`) are dropped — the MCP protocol says not to reply to them.

### Step 3: Method dispatch

A `match` on `method` handles `initialize`, `ping`, `tools/list`, and `tools/call`. For `tools/list`, orca returns its own tool definitions (from `dispatch::mcp_definitions()`) merged with any federated servers' tools.

### Step 4: `tools/call` routing

For `tools/call`, `serve()` picks one of a few routes by tool name:

1. **Plugin tools** — a dotted name that `is_plugin_tool` recognizes is forwarded to the daemon, which dispatches it via the in-process plugin registry (plugins are out-of-process; the daemon owns them).
2. **Federated tools** — names present in the in-memory `tool_registry` are forwarded to the owning MCP server over the `McpPool`.
3. **Orca's own tools** — when `dispatch::names()` contains the name, the call goes to `dispatch::dispatch_text(name, args, ctx)`. This is the `#[orca_tool]` path: the dispatch runtime looks the name up in its inventory-backed cache and invokes the erased tool. Reserved keys (peer target, correlation id) are stripped from `arguments` and folded onto a per-call `ToolCtx` clone before the call.
4. **Fallback** — anything else is offered to the live daemon, and only then to legacy Context7 federation.

The tool surface is projected from the `#[orca_tool]` inventory (see [patterns §4](02-patterns.md#4-macro-driven-tool-dispatch-orca_tool)), so `dispatch::names()` and `dispatch::dispatch_text()` are the seam that routes every one of orca's own tools.

### Step 5: Response written to stdout

The JSON-RPC response is serialized to a single newline-terminated line and flushed to stdout. Claude Code reads it and delivers the tool result into its context.

**Files touched:**
```
mcp/mod.rs:serve()                     ← stdin loop, method + tools/call routing
dispatch/src/registry.rs               ← names(), dispatch_text() over the tool inventory
<domain>/src/…  (the #[orca_tool] fn)  ← the actual tool logic
```

---

## Flow 2: An HTTP API Request

Any HTTP client hits the server built by `build_router()` in [`projects/server/src/serve/mod.rs`](../../projects/server/src/serve/mod.rs).

### Step 0: where the request comes from

A `curl`, the Scalar API viewer, or another pod peer can call `/api/v1/<name>`
directly. The web dashboard reaches it the same way: the external
[peacock](https://github.com/argyle-labs/peacock) plugin renders the UI, and its
generated typed client turns a call like `systemUpdate` into a POST to
`/api/v1/system.update` (adding `X-Orca-Peer` to target a remote pod peer). One
`#[orca_tool]` declaration serves that HTTP route, the MCP tool, and the CLI
subcommand at once — so every one of these callers lands on the same dispatch.
Peacock owns its own client and route files; this doc picks the request up at
axum.

### Step 1: axum router

`build_router(dev, db_path)` assembles the route tree: a handful of fixed routes (`/api/health`, `/api/openapi.json`, `/api/catalog`, auth, …) plus — the important part — the **entire tool surface** mounted under `/api/v1` via `dispatch::axum_router(ctx)`. That call emits one POST route per `#[orca_tool]`, so the same inventory that backs the MCP and CLI surfaces backs HTTP too. The router carries shared state (the `McpPool`) and CORS + correlation-id layers.

### Step 2: Middleware runs

Before the handler: the `log_requests` middleware (in [`serve/middleware.rs`](../../projects/server/src/serve/middleware.rs)) resolves a correlation id — reusing an inbound `x-correlation-id` header or minting a fresh one with `utils::id::new` — carries it as a `CorrelationId` extension, and logs the request/response around the handler; the CORS layer adds its headers. A per-request `x-orca-peer` header, when present, routes a `/api/v1/<name>` call to a remote pod peer instead of running it locally.

### Step 3: Handler runs

- A fixed route like `GET /api/health` calls its small handler (`ping_handler`) and returns JSON directly.
- A `/api/v1/<name>` route runs the tool through the same dispatch runtime as Flow 1 — the HTTP body is the tool's args, and the tool's typed output is serialized to the response.

### Step 4: Response serialized

Handlers return a type that implements `IntoResponse`; axum serializes it (JSON for data routes) with the right status and headers and sends it to the client.

**Files touched:**
```
serve/mod.rs:build_router()   ← route tree + /api/v1 tool mount
serve/middleware.rs           ← correlation-id injection
dispatch/src/registry.rs      ← axum_router() projects one route per tool
```

---

## Flow 3: A Chat Message in a Session

The user types a message in the TUI (or classic readline mode). Sessions live in the `conversation` crate under [`projects/conversation/src/sessions/`](../../projects/conversation/src/sessions/).

### Step 1: Session starts

With no subcommand, `main.rs` resolves a `ProjectContext` (from an explicit `--project`, an auto-detected cwd, or empty) and constructs a `Session` via `Session::new` (in [`sessions/session/mod.rs`](../../projects/conversation/src/sessions/session/mod.rs)). `new` loads config, builds the model backend with `build_backend`, and prepares the system prompt through `ProjectContext::build_system_prompt`. It then runs `run_tui()` (or `run()` in classic mode).

### Step 2: User input read

The TUI renders the split-pane UI and reads keystrokes; classic mode reads lines from stdin. Either way the user's text reaches the session's `chat()` method (in [`sessions/session/chat.rs`](../../projects/conversation/src/sessions/session/chat.rs)) as a `String`.

### Step 3: Message added to history

The session holds a `Vec<Message>` of conversation history. The user's input is appended as a user message.

### Step 4: `ModelBackend::chat()` called

`chat()` calls `self.backend.chat(...)`, passing the full history, the tool definitions, the system prompt, a `CancellationToken`, and the `OutputSink`. `self.backend` is a `Box<dyn ModelBackend>` whose concrete type (Claude / LM Studio / Ollama) was fixed at session creation. The backend serializes the messages to its provider's format and issues a streaming request — for `ClaudeBackend`, a streaming POST to the Anthropic Messages API (see [`projects/model/src/backend/claude.rs`](../../projects/model/src/backend/claude.rs)).

### Step 5: Stream parsed, tokens written to output

The backend reads server-sent events and writes each token to the `OutputSink` as it arrives. That is why output streams live in the terminal instead of appearing all at once.

### Step 6: Tool calls dispatched (if any)

When the model stops with a tool-use request, the session executes the requested tool locally, appends the result to the history as a tool-result message, and calls `chat()` again with the extended history. This loops until the model ends its turn.

### Step 7: Response appended to history

The model's final text is appended to the history as an assistant message, and the session loops back for the next input.

**Files touched:**
```
server/src/main.rs                        ← entry point, ProjectContext + Session::new
conversation/src/sessions/session/mod.rs  ← Session, new(), run()/run_tui()
conversation/src/sessions/session/chat.rs ← conversation loop, tool dispatch
model/src/backend/mod.rs                  ← ModelBackend trait, OutputSink
model/src/backend/claude.rs               ← HTTP call, stream parsing
```

---

## Reading Tip

The fastest way to understand a flow you haven't traced before:

1. Start at the entry point (`main.rs` for CLI, `mcp/mod.rs` for MCP, `serve/mod.rs` for HTTP).
2. Follow the function calls with `grep` or LSP "go to definition".
3. Look for the `Result<T>` return type — that tells you where errors are converted to responses.
4. Look for `.await` — that tells you where the flow suspends and what it is waiting for.
