# Hot Paths

Three code flows matter most for day-to-day development. This document traces each one from entry point to response, naming every function and file along the way. Read it with the source open.

---

## Flow 1: A Tool Call from Claude Code

Claude Code calls an orca tool (e.g., `get_config`) via the MCP protocol. Here is the full path.

### Step 1: stdin arrives

`orca mcp-serve` starts `mcp::serve()` in `projects/server/src/mcp/mod.rs`:

```rust
// projects/server/src/mcp/mod.rs:43
let stdin = tokio::io::stdin();
let mut lines = BufReader::new(stdin).lines();
let mut out = tokio::io::BufWriter::new(stdout);

while let Some(line) = lines.next_line().await? {
```

Claude Code writes a JSON-RPC line to the subprocess stdin. `next_line().await` returns when a full line arrives.

### Step 2: JSON-RPC parsing

```rust
// projects/server/src/mcp/mod.rs:52
let req: Value = match serde_json::from_str(&line) {
    Ok(v)  => v,
    Err(_) => continue,
};

let id     = req.get("id").cloned().unwrap_or(Value::Null);
let method = req["method"].as_str().unwrap_or("");
let params = req.get("params").cloned().unwrap_or(Value::Null);
```

The line is parsed as untyped JSON (`serde_json::Value`). The id, method, and params are extracted. Notifications (requests without an `id`) are silently dropped — the MCP protocol says not to reply to them.

### Step 3: Method dispatch

```rust
// projects/server/src/mcp/mod.rs:163
"tools/call" => {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];

    if name.contains('.') && is_plugin_tool(name) {
        // Plugin-declared tool — forward to the daemon, which
        // dispatches via the in-process PluginRegistry.
        match call_plugin_tool(name, args).await { /* reply */ }
    } else if dispatch::names().contains(&name) {
        // Orca's own registry tool.
        let result = dispatch::dispatch_text(name, args.clone(), &tool_ctx).await;
        match result {
            Ok(text) => reply(id, json!({ "content": [{ "type": "text", "text": text }], "isError": false })),
            Err(e)   => reply(id, json!({ "content": [{ "type": "text", "text": format!("Error: {e}") }], "isError": true })),
        }
    } else {
        /* unknown tool error */
    }
}
```

Plugin-declared tools (`<plugin_id>.<tool>`) are forwarded to the daemon's `PluginRegistry`; everything else is looked up in the inventory-backed registry that every `#[orca_tool]` function submitted into at link time.

### Step 4: Tool dispatch

`dispatch::dispatch_text` (in `projects/dispatch/`) finds the registration whose `domain.verb` name matches by walking `inventory::iter::<ToolRegistration>`, deserializes `args` into the tool's typed argument struct, and awaits the tool function. There is no hand-written per-tool match anywhere — the `#[orca_tool]` macro generated the erased wrapper.

### Step 5: Tool function runs

```rust
// e.g. projects/pod/src/lib.rs:1123
#[orca_tool(domain = "pod", verb = "list")]
async fn pod_list(_args: EmptyArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<PodListOutput> {
    // ... does its work, returns a typed output struct
}
```

The tool takes typed args, does its work (filesystem access, DB query, etc.), and returns a typed output struct — serialized to text for the MCP reply.


### Step 6: Response written to stdout

```rust
// projects/server/src/mcp/mod.rs:213
let mut payload = serde_json::to_string(&response)?;
payload.push('\n');
out.write_all(payload.as_bytes()).await?;
out.flush().await?;
```

The JSON-RPC response is serialized to a single line (newline-terminated) and flushed to stdout. Claude Code reads it and delivers the tool result to its context.

**Summary of files touched:**
```
mcp/mod.rs:serve()                ← stdin read loop + method dispatch
projects/dispatch/               ← inventory registry + typed dispatch
projects/<domain>/src/lib.rs     ← the #[orca_tool] function itself
```

---

## Flow 2: A Browser API Request

The browser (or any HTTP client) makes a `GET /api/health` request. Here is the path.

### Step 1: axum router

The router is built in `serve/mod.rs` by `build_router()`. Hand-written spec'd routes (auth) come from `openapi::openapi_router()`; infrastructure routes are added directly; and the entire `#[orca_tool]` registry is mounted under `/api/v1`:

```rust
// projects/server/src/serve/mod.rs:1239 (build_router, excerpt)
let (api, spec) = openapi::openapi_router().split_for_parts();
openapi::install_spec(spec);

let api = api
    .route("/api/health", get(ping_handler))
    .route("/api/openapi.json", get(openapi::openapi_handler))
    .route("/scalar", get(scalar_handler))
    .route("/api/auth/bootstrap", get(bootstrap_status_handler))
    .with_state(());

// Mount the OrcaTool registry under /api/v1. Same registry as MCP stdio
// and CLI — one trait impl, three live surfaces (REST + MCP + CLI).
let ctx = Arc::new(crate::mcp::build_tool_ctx(cfg));
let api = api.nest("/api/v1", dispatch::axum_router(ctx));
```

axum compiles the route tree. When a request arrives, axum matches the path and method, then calls the registered handler function.

### Step 2: Middleware runs

Before the handler, middleware runs (`serve/middleware.rs`):

- correlation-ID middleware — reads or generates `x-correlation-id`, injects `Extension(CorrelationId(id))`, and tags every request/response log line with it.
- auth middleware — resolves the session/token into an `AuthIdentity` extension (open paths like `/api/health` skip it).
- `CorsLayer` — adds CORS headers.

### Step 3: Handler is called

For `GET /api/health`:

```rust
// projects/server/src/serve/mod.rs:535
async fn ping_handler() -> axum::Json<Health> {
    axum::Json(Health { ok: true })
}
```

This handler takes no parameters (no state needed). `Json(...)` serializes the value and sets `Content-Type: application/json`.

For a tool endpoint like `POST /api/v1/pod/list`, `dispatch::axum_router` finds the registered `#[orca_tool]` by `domain/verb`, deserializes the body into the tool's typed args, and awaits the tool function — the exact same code path the MCP and CLI surfaces use.

### Step 4: Response serialized

The tool's typed output struct (which derives `Serialize`) is serialized to JSON and converted into an axum `Response` with the right status code and headers. axum sends it to the client.

**Summary of files touched:**
```
serve/mod.rs:build_router()      ← route registration + /api/v1 mount
serve/middleware.rs              ← correlation ID + auth injection
projects/dispatch/               ← typed dispatch to the tool function
projects/<domain>/src/lib.rs     ← the #[orca_tool] function
```
---

## Flow 3: A Chat Message in a Session

The user types a message in the TUI or classic readline mode. Here is the path from keystroke to model response.

### Step 1: Session starts

In `main.rs` with no subcommand:

```rust
// projects/server/src/main.rs:280
let ctx = ProjectContext::resolve(&project, &config)?;
let mut session = Session::new(config, ctx).await?;
session.run_tui().await
```

`Session::new` loads config, builds the model backend, and loads the agent's system prompt via `ctx.build_system_prompt(config)`.

### Step 2: User input read

In TUI mode, `session.run_tui()` renders the split-pane UI and reads keystrokes. In classic mode, `session.run()` reads lines from stdin. Either way, the user's text eventually reaches the session's message loop as a `String`.

### Step 3: Message added to history

The session maintains a `Vec<Message>` of conversation history. The user's input is appended as a `Message::user(text)`.

### Step 4: `ModelBackend::chat()` called

```rust
// session.rs (approximately)
let response = self.backend.chat(
    &self.messages,
    &self.tools,
    &system_prompt,
    self.cancel.clone(),
    &self.output,
).await?;
```

`self.backend` is a `Box<dyn ModelBackend>`. The actual type (Claude or LM Studio) was determined at session creation time. The session calls `.chat()` and awaits the response.

For `ClaudeBackend`:

```rust
// projects/model/src/backend/claude.rs:53
fn chat<'a>(&'a self, messages, tools, system, cancel, output) -> BoxFuture<'a, Result<BackendResponse>> {
  Box::pin(async move {
    let body = json!({
        "model": self.model,
        "max_tokens": 8192,
        "system": system,
        "messages": serialize::anthropic_messages(messages),
        "stream": true,
    });

    let response = self.client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &self.api_key)
        .json(&body)
        .send().await
        .context("failed to connect to Anthropic API")?;

    parse_claude_stream(response, cancel, output).await
  })
}
```

The messages are serialized to Anthropic's format, sent as a streaming POST request.

### Step 5: Stream parsed, tokens written to output

`parse_claude_stream` reads server-sent events from the response body. For each token event, it calls `sink_write(output, token)` — writing the token directly to the `OutputSink`. This is how streaming appears in the terminal: tokens print as they arrive, not all at once.

### Step 6: Tool calls dispatched (if any)

When the model returns `stop_reason: "tool_use"`, it means the model is requesting a tool call. The session extracts the tool name and arguments from `BackendResponse` and dispatches locally:

```rust
// session.rs (approximately)
if response.stop_reason == StopReason::ToolUse {
    for tool_call in &response.tool_calls {
        let result = self.execute_tool(&tool_call.name, &tool_call.input).await;
        // Append tool result to messages, loop back to chat()
    }
}
```

The result is appended to the conversation history as a tool result message, and `chat()` is called again with the updated history. This continues until the model returns `stop_reason: "end_turn"`.

### Step 7: Response appended to history

The model's final response text is appended to `self.messages` as `Message::assistant(text)`. The session loops back to read the next user input.

**Summary of files touched:**
```
main.rs                           ← entry point, SessionNew
server/src/session.rs             ← conversation loop, tool dispatch
model/src/backend/mod.rs           ← ModelBackend trait, OutputSink
model/src/backend/claude.rs        ← HTTP call, stream parsing
model/src/backend/serialize.rs     ← message format conversion
```

---

## Reading Tip

The fastest way to understand a flow you haven't traced before:

1. Start at the entry point (`main.rs` for CLI, `mcp/mod.rs` for MCP, `serve/mod.rs` + `dispatch::axum_router` for HTTP)
2. Follow the function calls with `grep` or LSP "go to definition"
3. Look for the `Result<T>` return type — that tells you where errors are converted to responses
4. Look for `.await` — that tells you where the flow suspends and what it is waiting for
