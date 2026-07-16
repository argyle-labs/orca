# Design Patterns

Orca uses a small set of recurring design patterns. Once you recognize them, the whole codebase becomes predictable: new features follow the same shapes as existing ones. This document names each pattern, shows where it appears, and explains why it exists.

---

## 1. Trait-Based Backend Selection

**Where:** `projects/model/src/backend/`

The model backend pattern separates the *interface* for talking to an LLM from the *implementation* for each specific model provider.

The trait:

```rust
// projects/model/src/backend/mod.rs:84
pub trait ModelBackend: Send + Sync {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        system: &'a str,
        cancel: CancellationToken,
        output: &'a OutputSink,
    ) -> BoxFuture<'a, Result<BackendResponse>>;

    fn name(&self) -> &str;
    fn model_id(&self) -> &str;
}
```

The factory:

```rust
// projects/model/src/backend/mod.rs:118
pub fn build_backend(config: &Config, model: &Model) -> Result<Box<dyn ModelBackend>> {
    match model {
        Model::Claude(id)           => Ok(Box::new(ClaudeBackend::new(key, id))),
        Model::LMStudio { id, url } => Ok(Box::new(LMStudioBackend::new(base, id))),
        Model::Ollama { id, url }   => Ok(Box::new(OllamaBackend::new(base, id))),
    }
}
```

The session code calls `backend.chat(...)` without knowing which backend it has. Three concrete types implement `ModelBackend` — `ClaudeBackend`, `LMStudioBackend`, and `OllamaBackend`. To add a new model provider (e.g., OpenAI), you implement `ModelBackend` for a new struct — a `fn chat<'a>(…) -> BoxFuture<'a, …>` returning `Box::pin(async move { … })` — and add a match arm in `build_backend`. Nothing else changes.

**The shape:** trait + factory function returning `Box<dyn Trait>` → callers use the trait, factory decides the concrete type.

---

## 2. Extension Injection (axum `Extension<T>`)

**Where:** `projects/server/src/serve/api/health.rs` and most API handlers

axum passes shared state to handlers via typed extensions. The router inserts state; handlers extract it by type.

Handler parameter:

```rust
// projects/server/src/serve/api/health.rs:41
pub async fn service_health_handler(
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
) -> Response {
```

`State(pool)` extracts the `McpState` (an `Arc<McpPool>`) that was registered on the router with `.with_state(pool)`. `Extension(CorrelationId(cid))` extracts the correlation ID that the middleware layer injected for this request.

axum's extractor system is type-driven: the handler declares what it needs as parameters, axum's compile-time machinery verifies the router was set up to provide them, and the runtime injects them.

**The shape:** middleware injects typed values into the request; handlers extract them by type from function parameters.

---

## 3. Embedded Resources via `rust-embed` and `build.rs`

**Where:** `projects/docs/`, `projects/agents/`, `projects/server/` (frontend)

Orca embeds all its assets — agent prompts, documentation, frontend HTML/JS/CSS — into the binary at compile time. No separate asset directories at runtime.

**`rust-embed` pattern** (for whole directories):

```rust
// docs/lib.rs:6
#[derive(rust_embed::RustEmbed)]
#[folder = "src"]
struct OrcaDocs;

// Access at runtime:
OrcaDocs::get("dev/00-tour.md")      // → Option<EmbeddedFile>
OrcaDocs::iter()                      // → iterator over all file paths
```

**`build.rs` pattern** (for code generation with `include_str!`):

```rust
// projects/agents/build.rs generates:
pub fn embedded_agent(name: &str) -> Option<&'static str> {
    match name {
        "wolf" => Some(include_str!("/path/to/wolf.md")),
        // ...
    }
}

// projects/agents/src/lib.rs includes it:
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));
```

The key difference: `rust-embed` puts files in a hashmap-like structure accessible by path. `build.rs` with `include_str!` creates a match arm per file — more explicit, easier to list at compile time.

**The shape:** compile-time embedding → docs/assets baked into the core binary (no external files for these), instant `O(1)` lookup.

---

## 4. JSON-RPC Dispatch Table

**Where:** `projects/server/src/mcp/mod.rs`

The MCP server receives a JSON-RPC request with a `method` field and dispatches to the appropriate handler. The dispatch table is a `match` on the method string:

```rust
// projects/server/src/mcp/mod.rs:66
let response = match method {
    "initialize" => reply(id, json!({ "protocolVersion": "2024-11-05", ... })),
    "ping"       => reply(id, json!({})),
    "tools/list" => { /* discover and list all tools */ }
    "tools/call" => {
        // Route to orca's tools OR federated server tools
        let result = dispatch(name, args, config).await;
        // ...
    }
    _ => error_reply(id, -32601, &format!("method not found: {method}")),
};
```

Within `tools/call`, a second dispatch table routes by tool name:

```rust
// projects/server/src/mcp/mod.rs:181
async fn dispatch(name: &str, args: &Value, config: &Config) -> Result<String> {
    match name {
        "list_agents"      => agents(),
        "get_agent"        => get_agent(args, config),
        "run_agent"        => run(args, config).await,
        "search_logs"      => search_logs(args, config),
        // ... 30+ entries
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}
```

**The shape:** string key → function call. Adding a new tool means adding one match arm in `dispatch` and one handler function. The name in the `match` is the name Claude Code calls.

The tool *definitions* (name, description, input schema) are declared separately in `projects/server/src/mcp/tools.rs` and returned by `tools/list`. The dispatch table and the tool definitions must stay in sync — if you add an arm to `dispatch`, you must also add an entry in `tools.rs`.

---

## 5. Builder/Context Assembly

**Where:** `projects/server/src/context.rs`

`ProjectContext` assembles a system prompt from multiple sources: an agent prompt (from the filesystem or embedded), and optional memory content (from the vault). The assembly is centralized in one method:

```rust
// projects/server/src/context.rs:54
pub fn build_system_prompt(&self, config: &Config) -> String {
    let wolf_prompt = orca_agents::load_agent_prompt("wolf", &config.agents_dir())
        .unwrap_or_else(|| {
            eprintln!("warning: wolf.md not found — using minimal fallback prompt");
            "You are an AI assistant. Be precise, efficient, and honest.".to_string()
        });

    if let Some(memory) = &self.memory_content {
        format!(
            "{}\n\n---\n\n## Project Context\n\nProject: {}\n\n{memory}",
            wolf_prompt,
            self.project.as_deref().unwrap_or("unknown"),
        )
    } else {
        wolf_prompt
    }
}
```

The `resolve` constructor is the builder:

```rust
// projects/server/src/context.rs:14
pub fn resolve(name: &str, config: &Config) -> Result<Self> {
    // exact match first, then fuzzy match, then empty context
    let exact = memory_root.join(name).join("MEMORY.md");
    if exact.exists() {
        let content = std::fs::read_to_string(&exact)?;
        return Ok(ProjectContext {
            project: Some(name.to_string()),
            memory_content: Some(content),
        });
    }
    // fuzzy...
    // fallback:
    Ok(ProjectContext { project: Some(name.to_string()), ..Default::default() })
}
```

**The shape:** a `resolve`/`new` function assembles state from multiple sources; a `build_*` method produces the final output. The struct carries intermediate state; the method produces the final artifact.

---

## 6. Registry Pattern

**Where:** MCP server registry (`orca.db`), schema registry, Docker runtime registry

Orca maintains several registries: external MCP servers, database schemas, Docker runtimes. Each follows the same structure:
- A SQLite table (via `orca_utils::db`) stores registered entries
- CLI subcommands (`add`, `remove`, `list`) manage the table
- MCP tools (`add_mcp_server`, `remove_mcp_server`, `list_mcp_servers`) expose the same operations
- HTTP endpoints (`/api/mcp/servers`) serve the registry to the frontend

The database access functions in `orca_utils::db` are thin wrappers:

```rust
// orca_utils/src/db.rs (approximately)
pub fn list_mcp_servers() -> Result<Vec<McpServerRow>> { ... }
pub fn add_mcp_server(row: &McpServerRow) -> Result<()> { ... }
pub fn remove_mcp_server(name: &str) -> Result<bool> { ... }
```

The HTTP handler wires to these with `db_json` / `db_ok` / `db_remove`:

```rust
// serve/api/mcp.rs (approximately)
pub async fn list_mcp_servers_handler() -> Response {
    db_json(|| orca_utils::db::list_mcp_servers())
}
```

And `db_json` handles the `Result` → `Response` conversion:

```rust
// projects/server/src/serve/api/mod.rs:17
pub fn db_json<T, F>(f: F) -> Response
where
    T: serde::Serialize,
    F: FnOnce() -> anyhow::Result<T>,
{
    match f() {
        Ok(val) => Json(val).into_response(),
        Err(e)  => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
```

**The shape:** SQLite table → CRUD functions in `orca_utils::db` → handler helpers (`db_json`, `db_ok`) → HTTP and MCP endpoints. Each new registry type follows the same five-step path.

---

## How the Patterns Compose

These patterns are not independent. In a typical feature, you will see several at once:

**Adding a new tool:**
1. **JSON-RPC dispatch** — add a match arm in `mcp/mod.rs::dispatch`
2. **Handler function** — add the logic in `mcp/handlers.rs` (returns `Result<String>`)
3. **Registry pattern** — if the tool reads from a DB table, use `db_json` in the corresponding HTTP handler
4. **Error handling** — `?` throughout, `.context()` for user-facing messages
5. **Module system** — export the handler from `handlers.rs`, import it in `mod.rs`

Each pattern is small and composable. When you see them together, they are not complexity — they are familiar structure.
