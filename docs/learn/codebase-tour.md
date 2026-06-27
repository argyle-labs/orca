# Codebase Tour

A guided walk through the orca binary — from a browser request to the Rust code that serves it, and from a Claude Code tool call to the Rust code that handles it.

---

## The roles

The `orca` binary does several things. You start it once and it handles all of them:

```
orca serve
  │
  ├─ Web server (HTTP)   :12000   axum — serves the SvelteKit app + REST API
  ├─ Web server (HTTPS)  :12443   axum — TLS for the same app + API
  └─ MCP server (stdio)  stdin    JSON-RPC 2.0 — `orca mcp-serve`, called by Claude Code
```

The SvelteKit UI is prerendered at build time and embedded into the binary via
`rust-embed` (the `Assets` struct in `projects/server/src/serve/mod.rs`).

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
  const root = parts[0] ?? 'orca';          // "docs"
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
projects/files/src/embedded.rs         (handler delegates here)
```

### 4. axum → rust-embed

The handler sees `root=docs` and delegates to `files::embedded::read("architecture")`:

```
projects/files/src/embedded.rs  →  OrcaDocs::get("architecture.md")
```

`OrcaDocs` is a `#[derive(rust_embed::RustEmbed)]` struct pointing at `docs/`. At compile time, every `.md` file in `docs/` was read from disk and baked into the binary as a static byte slice. At runtime, `OrcaDocs::get(...)` does a hashmap lookup — zero filesystem I/O.

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

When you use `orca_get_config` or `read_doc` inside Claude Code, here's what happens:

### 1. Claude Code → orca process

Claude Code spawned `orca mcp-serve` at startup (registered via `claude mcp add orca-local -- orca mcp-serve`). Claude writes a JSON-RPC request to the process's stdin:

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

`read_doc` calls `files::embedded::read("architecture")` — same function as the web path above. Result is wrapped in a JSON-RPC response and written to stdout.

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
- `orca serve` → starts the axum HTTP/HTTPS server
- `orca mcp-serve` → starts the MCP stdio loop
- `orca dev-serve` → serves the locally-built linux binary for fleet hot-reload
- All other (`orca <domain> <verb> …`) → passthrough subcommands dispatched within `projects/server/src/`

### HTTP layer

```
projects/server/src/serve/mod.rs          axum router + embedded SvelteKit Assets
projects/server/src/serve/openapi.rs      OpenAPI route registration + Scalar viewer
projects/server/src/serve/auth_routes.rs  auth/session endpoints
projects/server/src/serve/middleware.rs   request middleware (auth, logging)
projects/files/src/embedded.rs            embedded docs tree + read + full-text search
```

### MCP layer

```
projects/server/src/mcp/mod.rs    JSON-RPC stdio dispatcher (`orca mcp-serve`)
projects/server/src/mcp/tools.rs  tool definitions + handlers
```

### LLM backend

```
projects/plugins/llm/    LLM backend plugin (loaded via the plugin ABI)
```

### Shared types & config

```
projects/utils/src/    shared types and helpers
projects/contract/     config schema/types (loads ~/.orca/orca.toml)
projects/db/           encrypted state DB (~/.orca/orca.db)
```

### Embedded content (compiled into binary)

```
docs/                          Project WHY docs — this learning system (OrcaDocs)
projects/plugins/agents/       agent definitions surfaced through the agents plugin
```

### Frontend (SvelteKit 2 + Svelte 5)

```
projects/frontend/src/app.html              HTML shell
projects/frontend/src/routes/               One directory per page (+page.svelte / +page.ts)
projects/frontend/src/lib/components/        Shared components (Sidebar, banners, primitives)
projects/frontend/src/lib/api/              Generated typed API client (never edit manually)
```

---

## Configuration

Two layers:

**Compile time:**
- `projects/files/src/embedded.rs` embeds docs via `rust-embed` (`OrcaDocs`)
- `projects/server/src/serve/mod.rs` embeds the prerendered SvelteKit app (`Assets`)
- `projects/server/build.rs` ensures the frontend build output exists

**Runtime:**
- `~/.orca/orca.toml` — config, loaded at startup, not bundled into the binary
- `~/.orca/orca.db` — encrypted state database
- Environment variables: `ANTHROPIC_API_KEY`, `LMSTUDIO_URL`, `ORCA_LOG`
- 1Password integration: `op run --env-file .env.orca.tpl --` injects secrets

---

## Data flow summary

```
Browser
  │  HTTP GET /api/doc?root=docs&path=architecture
  ↓
axum router  (serve/mod.rs)
  │  route matches → handler
  ↓
doc handler
  │  root="docs" → files::embedded::read()
  │  root="orca" → filesystem tree (vault)
  ↓
files::embedded::read()  (projects/files/src/embedded.rs)
  │  OrcaDocs::get("architecture.md") → static bytes
  ↓
200 OK + markdown text
  ↓
[...slug]/+page.ts load → +page.svelte
  │  marked(content) → HTML
  ↓
Browser renders
```

---

## Where to go next

- [`rust-primer`](learn/rust-primer) — understand the Rust syntax in the files above
- [`svelte-primer`](learn/svelte-primer) — understand the Svelte 5 component patterns
- [`frontend-guide`](learn/frontend-guide) — add a new page or API endpoint yourself
- `projects/server/src/serve/api/` — browse the full handler directory
