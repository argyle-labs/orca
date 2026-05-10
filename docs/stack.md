# Technology Stack

Every technology choice is listed here with the reason it was selected and any known alternatives that were considered.

---

## Language — Rust

**Why:** Single compiled binary, no runtime dependency at the install target, native async for streaming LLM responses, and strong type safety across the CLI/TUI/server boundary. The same process handles the TUI, HTTP server, and MCP server without any IPC overhead.

**Alternatives considered:** Go (simpler concurrency but weaker type system, binary size similar), Python (fast iteration but requires runtime at install).

---

## Async Runtime — tokio

**Why:** Tokio is the de-facto standard async runtime in Rust. axum, reqwest, and the MCP stdio handling all require async. `tokio::process` handles subprocess spawning for docker/bash tools.

**Version:** `tokio = { version = "1", features = ["full"] }`

---

## HTTP Server — axum

**Why:** First-class tokio integration (shares the same async runtime, no bridge layer), type-safe extractors, and excellent middleware support via tower. The alternatives (actix-web, warp) would have required async bridging.

**Version:** `axum = { version = "0.7", features = ["macros"] }`

---

## CLI Parsing — clap

**Why:** Derive macros generate the full CLI from struct annotations with zero boilerplate. `--help` output, subcommands, and validation are handled automatically.

**Version:** `clap = { version = "4", features = ["derive"] }`

---

## HTTP Client — reqwest

**Why:** Async-first, streaming support for LLM token streaming (`features = ["stream"]`), and JSON deserialization integration. Used by both the LM Studio backend and the `brain_list_services` MCP tool.

**Version:** `reqwest = { version = "0.12", features = ["json", "stream"] }`

---

## Serialization — serde + serde_json + toml

**Why:** Serde is universal in the Rust ecosystem. `serde_json` for all API payloads, MCP JSON-RPC, and LLM request/response. `toml` for `brain.toml` config loading.

---

## Asset Embedding — rust-embed

**Why:** Compiles `site/dist/` and `docs/` into the binary at build time so the binary ships alone. No filesystem dependency at the install destination. See [single-binary.md](single-binary.md).

**Version:** `rust-embed = "8"`

---

## Terminal UI — ratatui + crossterm

**Why:** `ratatui` provides a retained-mode TUI framework (split panes, scrollable regions). `crossterm` handles cross-platform terminal I/O and event streaming. The split-pane TUI is the default interactive mode; `--classic` falls back to rustyline readline.

**Versions:** `ratatui = "0.29"`, `crossterm = { version = "0.28", features = ["event-stream"] }`

---

## Readline — rustyline

**Why:** History, line editing, and completion for the `--classic` interactive mode. Lighter than a full TUI when the user just wants a prompt.

**Version:** `rustyline = "14"`

---

## OpenAPI Generation — utoipa

**Why:** Proc macros on handler functions (`#[utoipa::path(...)]`) keep the spec in sync with the implementation. No separate schema file to maintain. The spec is assembled at runtime and served at `/api/openapi.json`.

**Version:** `utoipa = { version = "4", features = ["axum_extras"] }`

---

## Error Handling — anyhow

**Why:** Ergonomic `?`-based error propagation across the entire codebase without defining custom error types for every function. Used everywhere except the HTTP layer where explicit `StatusCode` responses are needed.

**Version:** `anyhow = "1"`

---

## Content Type Detection — mime_guess

**Why:** The embedded asset server needs to set the correct `Content-Type` header for every embedded file (JS, CSS, HTML, SVG, etc.) without hardcoding extension mappings.

**Version:** `mime_guess = "2"`

---

## Logging — tracing + tracing-subscriber

**Why:** Structured async-aware logging. Off by default in production; enabled via `BRAIN_LOG=debug`. Does not pollute the TUI output.

---

## Utilities

| Crate | Why |
|-------|-----|
| `dirs = "5"` | Portable home directory resolution — no hardcoded `/home/user/` |
| `uuid = "1"` | Session ID generation |
| `chrono = "0.4"` | Timestamps in session logs (with serde for JSONL serialization) |
| `glob = "0.3"` | File pattern matching in the grep/glob tools |
| `regex = "1"` | Full-text search query matching + `regex::escape()` to prevent injection |
| `colored = "2"` | Terminal color output in CLI mode |
| `tokio-util = "0.7"` | Cancellation tokens for interruptible tool execution |

---

## Frontend Language — TypeScript 6

**Why:** Type safety across the API boundary. The `gen` script produces typed client functions and types from the OpenAPI spec so the frontend and backend can't drift.

**Version:** `typescript = "^6.0.3"`

---

## Frontend Build Tool — Vite 8

**Why:** Vite 8 uses Rolldown (a Rust-based bundler) for significantly faster builds than Rollup/webpack. ESM-native. HMR for dev. The upgrade from Vite 6 → 8 happened when Rolldown stabilized.

**Version:** `vite = "^8.0.10"`

---

## UI Framework — Svelte 5 + SvelteKit 2

**Why:** Svelte compiles components to vanilla JS — no virtual DOM, smaller runtime, faster updates. Svelte 5's rune system (`$state`, `$derived`, `$effect`, `$props`) makes reactivity explicit and debuggable. SvelteKit adds file-based routing, typed load functions, and a static adapter that produces a pure client-side bundle that embeds cleanly into the Rust binary.

**Why not React:** The React ecosystem adds TanStack Router + TanStack Query + Mantine on top of React itself — four dependencies to do what SvelteKit handles natively. Svelte components compile to smaller output with less client-side overhead.

**Versions:** `svelte = "^5.55.5"`, `@sveltejs/kit = "^2.59.0"`, `@sveltejs/adapter-static = "^3.0.10"`

---

## Graph Visualization — @xyflow/svelte

**Why:** Interactive node/edge graphs for visualizing service dependencies and doc relationships. The Svelte port of the XyFlow (formerly ReactFlow) library — same layout primitives, Svelte-native component integration.

**Version:** `@xyflow/svelte = "^0.1.39"`, `@dagrejs/dagre = "^3.0.0"` (layout algorithm)

---

## API Documentation Viewer — @scalar/sveltekit

**Why:** Renders the OpenAPI spec at `/api-docs` with a polished interactive UI. The SvelteKit integration (`@scalar/sveltekit`) drops in as a SvelteKit route handler — no adapter layer needed.

**Version:** `@scalar/sveltekit = "^0.2.13"`

---

## Markdown Rendering — marked

**Why:** Converts markdown strings to HTML for the doc viewer. Lightweight synchronous parser (no VDOM dependency like react-markdown). Used in `[...slug]/+page.svelte` via `{@html marked(content)}`.

**Version:** `marked = "^15.0.12"`

---

## Script Runner — tsx

**Why:** Executes TypeScript scripts directly without a separate compile step. Used by the `gen` script that calls the OpenAPI spec and generates the typed API client.

**Version:** `tsx = "^4.21.0"`
