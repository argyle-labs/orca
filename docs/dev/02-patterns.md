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

**Where:** `projects/server/src/serve/auth_routes.rs` and the middleware layer

axum passes shared state to handlers via typed extensions. Middleware inserts values; handlers extract them by type.

Handler parameter:

```rust
// projects/server/src/serve/auth_routes.rs:428
pub async fn change_password(
    axum::extract::Extension(ident): axum::extract::Extension<AuthIdentity>,
    Json(body): Json<ChangePasswordRequest>,
) -> Response {
```

`Extension(ident)` extracts the `AuthIdentity` that the auth middleware injected for this request; `Json(body)` deserializes the request body into a typed struct. The middleware in `serve/middleware.rs` similarly injects a `CorrelationId` per request for log tracing.

axum's extractor system is type-driven: the handler declares what it needs as parameters, axum's compile-time machinery verifies the router was set up to provide them, and the runtime injects them.

**The shape:** middleware injects typed values into the request; handlers extract them by type from function parameters.

---

## 3. Embedded Resources via `rust-embed` and `build.rs`

**Where:** `projects/files/` (docs), `projects/contract/` (config-docs), `projects/server/` (frontend)

Orca embeds its static assets — documentation, config docs, frontend HTML/JS/CSS — into the binary at compile time. No separate asset directories at runtime.

**`rust-embed` pattern** (for whole directories):

```rust
// projects/files/src/embedded.rs:10
#[derive(rust_embed::RustEmbed)]
#[folder = "../../docs"]
struct OrcaDocs;

// Access at runtime:
OrcaDocs::get("dev/00-tour.md")      // → Option<EmbeddedFile>
OrcaDocs::iter()                      // → iterator over all file paths
```

Agent prompts are **not** embedded — they are contributed at runtime by plugins registering an `AgentProvider` (`projects/contract/src/agents.rs`); `orca install` materializes whatever providers registered into `~/.claude/agents/`.

**The shape:** compile-time embedding → single binary, no external files, instant `O(1)` lookup.

---

## 4. JSON-RPC Dispatch Table

**Where:** `projects/server/src/mcp/mod.rs`

The MCP server receives a JSON-RPC request with a `method` field and dispatches to the appropriate handler. The dispatch table is a `match` on the method string:

```rust
// projects/server/src/mcp/mod.rs:110
let response = match method {
    "initialize" => reply(id, json!({ "protocolVersion": "2024-11-05", /* … */ })),
    "ping"       => reply(id, json!({})),
    "tools/list" => { /* registry-derived defs + plugin-declared tools */ }
    "tools/call" => { /* route through the inventory-backed dispatcher */ }
    _ => error_reply(id, -32601, &format!("method not found: {method}")),
};
```

Within `tools/call` there is **no hand-written per-tool match**. Every `#[orca_tool]` function in a domain crate submits a `ToolRegistration` into an `inventory` slice at link time (`projects/dispatch/src/inventory_slice.rs`), and dispatch walks `inventory::iter` to find the tool by `domain.verb` name. `tools/list` is likewise generated from the registry (`dispatch::mcp_definitions()`), so definitions and dispatch can never drift apart.

**The shape:** annotate a function with `#[orca_tool(domain = "...", verb = "...")]` → it appears on MCP, HTTP, CLI, and OpenAPI automatically. Adding a tool is one function, zero registration boilerplate.

---

## 5. Builder/Context Assembly

**Where:** `projects/conversation/src/sessions/context.rs`

`ProjectContext` assembles a system prompt from multiple sources: an agent prompt (from the filesystem or embedded), and optional memory content (from the vault). The assembly is centralized in one method:

```rust
// projects/conversation/src/sessions/context.rs:54
pub fn build_system_prompt(&self, config: &Config) -> String {
    self.build_system_prompt_for_backend(config, true)
}

pub fn build_system_prompt_for_backend(&self, _config: &Config, full_persona: bool) -> String {
    let base = if full_persona {
        contract::agents::load_agent_prompt("wolf").unwrap_or_else(|| {
            eprintln!("warning: wolf.md not found — using minimal fallback prompt");
            "You are an AI assistant. Be precise, efficient, and honest.".to_string()
        })
    } else {
        // Local models (LMStudio, Ollama) get a clean minimal prompt.
        local_model_prompt()
    };

    if let Some(memory) = &self.memory_content {
        format!(
            "{}\n\n---\n\n## Project Context\n\nProject: {}\n\n{memory}",
            base,
            self.project.as_deref().unwrap_or("unknown"),
        )
    } else {
        base
    }
}
```

The `resolve` constructor is the builder:

```rust
// projects/conversation/src/sessions/context.rs:14
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

Orca maintains several registries: external MCP servers, database schemas, plugins, models. Each follows the same structure:
- A SQLite table (via the `db` crate) stores registered entries — **every persistent table's CRUD lives in `db`**, never inline SQL in a domain crate
- `#[orca_tool]` functions in the owning domain crate (`mcp.list`, `mcp.update`, `model.list`, …) expose the five-verb surface (list/detail/create/update/delete)
- The one annotation makes the same operation available on CLI, MCP, HTTP, and OpenAPI

```rust
// the shape (domain crate, e.g. the mcp plugin or projects/model)
#[orca_tool(domain = "model", verb = "list")]
async fn model_list(_args: EmptyArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<ModelListOutput> {
    let rows = db::models::list()?;   // CRUD lives in the db crate
    Ok(ModelListOutput { rows })
}
```

There are also process-global capability registries for runtime contributions (agents, cluster rosters, storage backends): a trait + `LazyLock<RwLock<Vec<Arc<dyn Provider>>>>` in `contract`, which plugins register into at load time — see `docs/CAPABILITY-REGISTRIES.md`.

**The shape:** SQLite table → CRUD in `db` → one `#[orca_tool]` per verb → every surface. Each new registry type follows the same path.

---

## How the Patterns Compose

These patterns are not independent. In a typical feature, you will see several at once:

**Adding a new tool:**
1. **Typed args/output** — define the input and output structs (`Serialize`/`Deserialize`/`JsonSchema`; no opaque JSON)
2. **`#[orca_tool]` function** — write the async fn in the owning domain crate with `#[orca_tool(domain = "...", verb = "...")]`
3. **Registry pattern** — if the tool reads a DB table, call the CRUD functions in the `db` crate
4. **Error handling** — `?` throughout, `.context()` for user-facing messages
5. **Done** — the inventory registration puts it on CLI, MCP, HTTP, and OpenAPI automatically

Each pattern is small and composable. When you see them together, they are not complexity — they are familiar structure.
