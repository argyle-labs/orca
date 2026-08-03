# Design Patterns

Orca uses a small set of recurring design patterns. Once you recognize them, the whole codebase becomes predictable: new features follow the same shapes as existing ones. This document names each pattern, shows where it appears, and explains why it exists.

---

## 1. Trait-Based Backend Selection

**Where:** [`projects/model/src/backend/`](../../projects/model/src/backend/)

The model backend pattern separates the *interface* for talking to an LLM from the *implementation* for each specific provider.

The trait is `ModelBackend` in [`projects/model/src/backend/mod.rs`](../../projects/model/src/backend/mod.rs). Its central method is `fn chat<'a>(…) -> BoxFuture<'a, Result<BackendResponse>>` — it takes the message history, tool definitions, a system prompt, a `CancellationToken`, and an `OutputSink`, and returns the model's response. The trait also exposes `name`, `model_id`, and an `is_local` predicate (cloud vs local).

The factory is `build_backend` in the same file. It matches on the `Model` enum and returns a `Box<dyn ModelBackend>`. Three concrete types implement the trait — `ClaudeBackend`, `LMStudioBackend`, and `OllamaBackend` (each in its own file under `backend/`). Session code calls `backend.chat(...)` without knowing which concrete type it holds.

To add a new provider (e.g. OpenAI), implement `ModelBackend` for a new struct — a `fn chat<'a>(…) -> BoxFuture<'a, …>` whose body is `Box::pin(async move { … })` — and add one arm to `build_backend`. Nothing else changes.

**The shape:** trait + factory function returning `Box<dyn Trait>` → callers use the trait, the factory decides the concrete type.

---

## 2. Extension Injection (axum `Extension<T>` / `State<T>`)

**Where:** the HTTP router in [`projects/server/src/serve/mod.rs`](../../projects/server/src/serve/mod.rs) and its middleware in [`projects/server/src/serve/middleware.rs`](../../projects/server/src/serve/middleware.rs)

axum passes shared state to handlers via typed extractors. The router inserts state and middleware injects per-request values; handlers extract them by type in their parameter list.

- `State(pool): State<…>` extracts the shared state registered on the router with `.with_state(...)` — for orca that state is the long-lived `McpPool` used to reach federated servers.
- `Extension(CorrelationId(cid)): Extension<CorrelationId>` extracts the correlation ID that the `correlation_id` middleware layer injected for this request.

axum's extractor system is type-driven: the handler declares what it needs as parameters, axum's compile-time machinery verifies the router provides them, and the runtime injects them.

**The shape:** middleware/router inject typed values into the request; handlers extract them by type from function parameters.

---

## 3. Embedded Resources via `rust-embed`

**Where:** [`projects/files/src/embedded.rs`](../../projects/files/src/embedded.rs) (docs), [`projects/agents/src/embedded.rs`](../../projects/agents/src/embedded.rs) (agent prompts)

Orca bakes some assets into the binary at compile time rather than reading them from disk at runtime.

The docs tree is embedded with `rust-embed`: `embedded.rs` derives `RustEmbed` over `#[folder = "../../docs"]`, exposing `read`, `search`, `tree`, and `list` helpers that [`projects/files/src/lib.rs`](../../projects/files/src/lib.rs) serves through the `files.*` tools. `rust-embed` stores files in a hashmap-like structure keyed by path, so lookup is `O(1)` and re-embedding happens automatically on every build.

Agent prompts use the same idea in [`projects/agents/src/embedded.rs`](../../projects/agents/src/embedded.rs), with a clear ownership boundary: core ships the resolution machinery (`load_agent_prompt`, `strip_frontmatter`) and resolves a prompt against whatever a plugin registers plus any files under `~/.orca/agents/`. The `wolf`/`otter`/… persona `.md` files live in the external `argyle-labs/agents` plugin, which contributes them over the `agents.register` capability.

**The shape:** compile-time embedding → assets baked into the binary, no external files at runtime, instant lookup.

---

## 4. Macro-Driven Tool Dispatch (`#[orca_tool]`)

**Where:** proc-macro crate [`projects/derive/`](../../projects/derive/), runtime crate [`projects/dispatch/`](../../projects/dispatch/)

A tool is a single annotated function. You write `#[orca_tool(domain = "…", verb = "…")]` above an `async fn` that takes a typed args struct and a `&ToolCtx` and returns a typed `Result`. The macro (in [`projects/derive/src/lib.rs`](../../projects/derive/src/lib.rs)) emits an `inventory` entry at compile time; the runtime (in [`projects/dispatch/`](../../projects/dispatch/)) walks that inventory once at startup and projects **every** surface from it:

- **MCP** — `mcp_definitions()` builds `tools/list`; `dispatch()` / `dispatch_text()` serve `tools/call`.
- **HTTP** — `axum_router(ctx)` mounts one POST route per tool.
- **CLI** — `clap_command()` + `cli_dispatch()` drive `orca exec <name>`.

All of these live in [`projects/dispatch/src/registry.rs`](../../projects/dispatch/src/registry.rs), backed by the `ToolRegistration` inventory slice in [`projects/dispatch/src/inventory_slice.rs`](../../projects/dispatch/src/inventory_slice.rs) and the type-erased `ErasedTool` wrapper in [`projects/dispatch/src/erased.rs`](../../projects/dispatch/src/erased.rs). The tool name (`<domain>.<verb>`), its JSON schema, and its handler all come from the one annotated function, so the inventory is the single source of truth for every surface.

The macro pair is split into `derive` (proc-macro) + `dispatch` (runtime) for the same reason `serde-derive` and `serde` are separate: a proc-macro crate exports only macros, so the runtime code lives in its own crate. Args and outputs cross the erased boundary as `serde_json::Value` (see the module docs in `registry.rs`), and callers downcast via serde immediately after dispatch.

**The shape:** annotate one function → one inventory walk projects MCP, HTTP, and CLI surfaces automatically. Adding a tool means adding a function; the inventory wires up every surface.

---

## 5. Builder/Context Assembly

**Where:** [`projects/conversation/src/sessions/context.rs`](../../projects/conversation/src/sessions/context.rs)

`ProjectContext` assembles a system prompt from multiple sources: the agent prompt (resolved from the filesystem, an embedded copy, or a plugin), plus optional project memory from the vault. The `resolve` constructor gathers state (matching a project name against `~/.orca/memory/<name>/MEMORY.md`, exact then fuzzy, else empty), and `build_system_prompt` produces the final string — loading the `wolf` prompt via `agents::resolve::load_agent_prompt` and appending the memory block when present.

**The shape:** a `resolve`/`new` function assembles state from several sources; a `build_*` method produces the final artifact. The struct carries intermediate state; the method emits the output.

---

## 6. Registry Pattern

**Where:** the runtime registry `orca.db` ([`projects/db/`](../../projects/db/)) plus the domain crates that own their tables

Orca keeps runtime state (MCP servers, schemas, plugins, mounts, …) in an encrypted SQLite database, `orca.db`. The [`db`](../../projects/db/) crate provides the thin primitives — pool, schema, migration, replication — while each **domain crate owns its own tables** and CRUD (the ongoing thin-`db` split; see [`CRATE_RESPONSIBILITIES.md`](../../CRATE_RESPONSIBILITIES.md)).

A domain typically exposes its operations as `#[orca_tool]` functions, which — per pattern 4 — automatically appear as CLI verbs, `/api/v1` routes, and MCP tools without any per-surface wiring.

**The shape:** SQLite table owned by a domain crate → `#[orca_tool]` functions over it → CLI, HTTP, and MCP surfaces projected by the dispatch runtime.

---

## How the Patterns Compose

These patterns are not independent. Adding a new tool touches several at once:

1. **Macro dispatch** — write one `#[orca_tool]` function (pattern 4). That is the entire wiring for CLI, HTTP, and MCP.
2. **Registry** — if it reads or writes runtime state, go through the owning domain crate's tables over `orca.db` (pattern 6).
3. **Error handling** — `?` throughout, `.context()` for user-facing messages; the dispatch runtime turns your `Result` into the right per-surface response.

Each pattern is small and composable. Seeing them together gives you familiar structure to build on.
