# Codebase Tour

A guided walk through the brain binary — from a browser request to the Rust code that serves it, and from a Claude Code tool call to the Rust code that handles it.

---

## The four roles

The `brain` binary does four things simultaneously. You start it once and it handles all of them:

```
brain serve
  │
  ├─ Web server      :12000  axum HTTP — serves React app + REST API
  ├─ MCP server      stdin   JSON-RPC 2.0 — called by Claude Code
  ├─ TUI             terminal  ratatui split-pane chat UI (default mode)
  └─ CLI REPL        terminal  readline chat for --classic mode
```

In development, you run `make dev` which starts cargo-watch (rebuilds on save) and Vite's HMR server on `:12001`. The Vite server proxies `/api/*` to `:12000`.

---

## Tracing a doc request

Let's follow what happens when you navigate to `/docs/architecture` in the browser.

### 1. Browser → SvelteKit router

SvelteKit matches `/docs/architecture`. No named route matches, so the catch-all `[...slug]` route fires.

```
projects/frontend/src/routes/[...slug]/+page.ts     ← load function
projects/frontend/src/routes/[...slug]/+page.svelte ← component
```

### 2. The load function parses the URL and fetches

SvelteKit calls the `load` function in `+page.ts` before rendering the component:

```typescript
// projects/frontend/src/routes/[...slug]/+page.ts
export const load: PageLoad = async ({ params }) => {
  const slug = params.slug ?? '';           // "docs/architecture"
  const parts = slug.split('/').filter(Boolean);
  const root = parts[0] ?? 'brain';         // "docs"
  const path = parts.slice(1).join('/');    // "architecture"

  const raw = await getDoc({ root, path }); // calls GET /api/doc?root=docs&path=architecture
  return { content: String(raw ?? ''), root, path };
};
```

`getDoc` is a generated function from `src/lib/api/client.ts` — typed, no raw `fetch()`.

### 3. +page.ts → GET /api/doc → axum

```
GET /api/doc?root=docs&path=architecture
```

This hits the axum router in:

```
projects/server/src/serve/openapi.rs   (route registration)
projects/server/src/serve/api/docs.rs  (handler implementation)
```

### 4. axum → rust-embed

The handler sees `root=docs` and delegates to `brain_docs::read("architecture")`:

```
docs/lib.rs  →  BrainDocs::get("architecture.md")
```

`BrainDocs` is a `#[derive(RustEmbed)]` struct. At compile time, every `.md` file in `docs/` was read from disk and baked into the binary as a static byte slice. At runtime, `BrainDocs::get(...)` does a hashmap lookup — zero filesystem I/O.

### 5. axum → load function → component

The handler returns `200 OK` with markdown text. The load function receives it and returns `{ content, root, path }` to the component.

### 6. +page.svelte renders

```svelte
<script lang="ts">
  import { marked } from 'marked';
  let { data } = $props();
  const html = $derived(data.content ? marked(data.content) : '');
</script>

<article class="doc">{@html html}</article>
```

`marked(data.content)` converts markdown to HTML. `{@html html}` renders it directly — Svelte bypasses escaping when you explicitly ask for raw HTML output.

---

## Tracing an MCP tool call

When you use `brain_get_context` or `read_doc` inside Claude Code, here's what happens:

### 1. Claude Code → brain process

Claude Code spawned `brain mcp-serve` at startup (registered via `claude mcp add brain-local -- brain mcp-serve`). Claude writes a JSON-RPC request to the process's stdin:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "read_doc",
    "arguments": { "root": "docs", "path": "architecture" }
  }
}
```

### 2. MCP dispatcher

```
projects/server/src/mcp/mod.rs
```

The dispatcher reads lines from stdin, parses each as JSON-RPC, and routes by `method`:

- `initialize` → returns server capabilities
- `tools/list` → returns all tool definitions
- `tools/call` → dispatches to the matching tool handler

### 3. Tool handler

`read_doc` calls `brain_docs::read("architecture")` — same function as the web path above. Result is wrapped in a JSON-RPC response and written to stdout.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": { "content": [{ "type": "text", "text": "# Architecture\n..." }] }
}
```

Claude Code reads this from the process's stdout and presents it as a tool result.

---

## Key files by concern

### Entry point

```
projects/server/src/main.rs
```

Parses CLI arguments with clap, then dispatches:
- `brain serve` → starts axum + optional TUI
- `brain mcp-serve` → starts the MCP stdio loop
- `brain chat` / no subcommand → starts the REPL/TUI
- All other subcommands → delegated to `projects/commands/src/`

### HTTP layer

```
projects/server/src/serve/mod.rs    axum router — maps routes to handlers
projects/server/src/serve/api.rs    all HTTP handlers
projects/server/src/serve/tree.rs   filesystem tree + full-text search (brain/rebuy vaults)
projects/server/src/serve/mcp_client.rs  spawns external MCP servers, proxies calls
```

### LLM backends

```
projects/core/src/backend/mod.rs        ModelBackend trait
projects/core/src/backend/lmstudio.rs   LM Studio (default, local)
projects/core/src/backend/claude.rs     Anthropic API (escalation)
```

### Shared types

```
projects/utils/src/types.rs     Message, ToolCall, ToolResult, BackendResponse
projects/utils/src/config.rs    Config — loads brain.toml
projects/utils/src/state.rs     DaemonState — port handoff file
```

### Embedded content (compiled into binary)

```
docs/       Project WHY docs — this learning system
projects/agents/build.rs Reads ~/brain/ai/claude/agents/*.md at compile time
```

### Frontend

```
projects/frontend/src/main.tsx         React root + providers
projects/frontend/src/routes/          One file per page
projects/frontend/src/components/      Shared components (MarkdownRenderer, Sidebar, etc.)
projects/frontend/src/contexts/        Global state (theme)
projects/frontend/src/api/             Generated typed API hooks (never edit manually)
```

---

## Configuration

Two layers:

**Compile time:**
- `projects/agents/build.rs` embeds agent .md files
- `projects/docs` embeds docs via rust-embed
- `projects/server/build.rs` ensures `frontend/dist/` exists

**Runtime:**
- `~/brain/config/brain.toml` — loaded at startup, not bundled into the binary
- Environment variables: `ANTHROPIC_API_KEY`, `LMSTUDIO_URL`, `BRAIN_LOG`
- 1Password integration: `op run --env-file .env.brain.tpl --` injects secrets

---

## Data flow summary

```
Browser
  │  HTTP GET /api/doc?root=docs&path=architecture
  ↓
axum router  (serve/mod.rs)
  │  route matches → handler
  ↓
api.rs handler
  │  root="docs" → brain_docs::read()
  │  root="brain" → serve/tree.rs (filesystem)
  │  root="rebuy" → serve/tree.rs (filesystem)
  ↓
brain_docs::read()  (projects/docs/src/lib.rs)
  │  BrainDocs::get("architecture.md") → static bytes
  ↓
200 OK + markdown text
  ↓
DocPage.tsx
  │  setContent(raw)
  ↓
MarkdownRenderer.tsx
  │  <ReactMarkdown> → HTML
  ↓
Browser renders
```

---

## Where to go next

- [`rust-primer`](learn/rust-primer) — understand the Rust syntax in the files above
- [`svelte-primer`](learn/svelte-primer) — understand the Svelte 5 component patterns
- [`frontend-guide`](learn/frontend-guide) — add a new page or API endpoint yourself
- `projects/server/src/serve/api/` — browse the full handler directory
