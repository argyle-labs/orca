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
// projects/server/src/mcp/mod.rs:66
let response = match method {
    "tools/call" => {
        let name = params["name"].as_str().unwrap_or("");
        let args = &params["arguments"];

        if let Some((server_name, internal_name)) = tool_registry.get(name).cloned() {
            // Route to a federated server
            ...
        } else {
            // Orca's own tool
            let result = dispatch(name, args, config).await;
            match result {
                Ok(text) => reply(id, json!({ "content": [{ "type": "text", "text": text }], "isError": false })),
                Err(e)   => reply(id, json!({ "content": [{ "type": "text", "text": format!("Error: {e}") }], "isError": true })),
            }
        }
    }
    // ...
};
```

The `tool_registry` (a `HashMap<String, (String, String)>`) maps federated tool names to their owning server. Tools not in the registry are orca's own — dispatched locally.

### Step 4: Tool dispatch

```rust
// projects/server/src/mcp/mod.rs:181
async fn dispatch(name: &str, args: &Value, config: &Config) -> Result<String> {
    match name {
        "get_config" => get_config(args, config),
        // ...
    }
}
```

One match arm per tool. For `get_config`, it calls `handlers::get_config`.

### Step 5: Handler runs

```rust
// projects/server/src/mcp/handlers.rs
pub fn get_config(args: &Value, config: &Config) -> Result<String> {
    let key = args["key"].as_str()
        .ok_or_else(|| anyhow::anyhow!("key is required"))?;
    // ... reads from orca config/vault, returns a string
    Ok(result_string)
}
```

The handler reads `args`, does its work (filesystem access, DB query, etc.), and returns `Result<String>`.

### Step 6: Response written to stdout

```rust
// projects/server/src/mcp/mod.rs:172
let mut payload = serde_json::to_string(&response)?;
payload.push('\n');
out.write_all(payload.as_bytes()).await?;
out.flush().await?;
```

The JSON-RPC response is serialized to a single line (newline-terminated) and flushed to stdout. Claude Code reads it and delivers the tool result to its context.

**Summary of files touched:**
```
mcp/mod.rs:serve()          ← stdin read loop + method dispatch
mcp/mod.rs:dispatch()       ← tool name → handler function
mcp/handlers.rs             ← actual tool logic
```

---

## Flow 2: A Browser API Request

The browser (or any HTTP client) makes a `GET /api/health` request. Here is the path.

### Step 1: axum router

The router is built in `serve/mod.rs` by `build_router()`. It registers all routes:

```rust
// projects/server/src/serve/mod.rs (build_router, approximately)
Router::new()
    .route("/api/health",              get(health::ping_handler))
    .route("/api/service/health/local",  get(health::service_health_handler))
    // ... many more
    .with_state(mcp_pool)
    .layer(CorsLayer::permissive())
    .layer(middleware::from_fn(middleware::correlation_id))
```

axum compiles the route tree. When a request arrives, axum matches the path and method, then calls the registered handler function.

### Step 2: Middleware runs

Before the handler, middleware runs:

- `correlation_id` middleware — generates a UUID, injects `Extension(CorrelationId(uuid))` into the request. Handlers can extract this for log correlation.
- `CorsLayer` — adds CORS headers.

### Step 3: Handler is called

For `GET /api/health`:

```rust
// projects/server/src/serve/api/health.rs:23
pub async fn ping_handler() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}
```

This handler takes no parameters (no state needed). It returns `impl IntoResponse` — axum will call `.into_response()` on whatever it returns. `Json(...)` serializes the value to JSON and sets `Content-Type: application/json`.

For `GET /api/service/health/local`:

```rust
// projects/server/src/serve/api/health.rs:39
pub async fn service_health_handler(
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
) -> Response {
```

axum extracts `McpState` from the router's state and `CorrelationId` from the middleware-injected extension. These become local variables inside the handler.

### Step 4: Handler does its work

For the health handler, it fans out to several MCP tool calls in parallel:

```rust
// projects/server/src/serve/api/health.rs:62
let futures: Vec<_> = CHECKS.iter().map(|(label, tool)| {
    let client = client.clone();
    async move {
        let result = client.call_tool(&tool, json!({}), &cid).await;
        HealthCheck { label, tool, output, ok }
    }
}).collect();

let checks = futures_util::future::join_all(futures).await;
```

All health checks run concurrently via `join_all`.

### Step 5: Response serialized

```rust
// projects/server/src/serve/api/health.rs:87
Json(HealthResponse {
    timestamp: chrono::Utc::now().to_rfc3339(),
    checks,
}).into_response()
```

`Json(...)` serializes `HealthResponse` (which derives `Serialize`) to JSON. `.into_response()` converts it to an axum `Response` with the right status code and headers. axum sends it to the client.

**Summary of files touched:**
```
serve/mod.rs:build_router()  ← route registration
serve/middleware.rs           ← correlation ID injection
serve/api/health.rs           ← handler implementation
serve/api/mod.rs              ← shared response helpers
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

1. Start at the entry point (`main.rs` for CLI, `mcp/mod.rs` for MCP, `serve/api/*.rs` for HTTP)
2. Follow the function calls with `grep` or LSP "go to definition"
3. Look for the `Result<T>` return type — that tells you where errors are converted to responses
4. Look for `.await` — that tells you where the flow suspends and what it is waiting for
