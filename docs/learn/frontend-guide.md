# Frontend Guide

Where the orca web dashboard lives and how orca serves it. For newcomers who
expect a frontend in this repo.

The dashboard is the **`peacock` plugin** — a SvelteKit 2 + Svelte 5 app in the
separate [argyle-labs/peacock](https://github.com/argyle-labs/peacock) repo. Its
source, page/component layout, generated API client, and contributor guide all
live there. This repo documents the orca side of the boundary only.

## How orca serves it

peacock is an out-of-process plugin that registers `contract::web` and owns
orca's root route `/`. `orca serve` proxies unmatched `/` requests to peacock's
`peacock.render` tool in production, or to peacock's Vite dev server in
development (`make dev`). The two release as separate artifacts: an orca release
and a peacock release.

peacock talks to orca through a typed client generated from orca's OpenAPI spec
via [`@hey-api/openapi-ts`](https://heyapi.dev/). Every function in that client
maps to one `#[orca_tool]` endpoint on orca's tool surface, so the frontend
consumes exactly the surface orca exposes.

## The orca-side task: expose a tool the dashboard can call

orca's REST/MCP surface is declared with the `#[orca_tool]` / `#[endpoint_tool]`
macros (proc-macro crate `derive`, runtime `dispatch`). The macro registers the
tool and contributes its schema to the OpenAPI spec the client is generated
from. To give peacock something new to call:

1. **Add a tool in the owning domain crate** under `projects/`, annotating the
   function with `#[orca_tool(domain = "...", verb = "...")]`. The macro derives
   the typed args/output schema. See existing tools such as
   `projects/system/src/config_tools.rs` for the pattern.
2. **Rebuild the orca binary** so the tool registers and appears in the OpenAPI
   spec served at runtime.
3. **Regenerate peacock's client** (`npm run gen:client` in the peacock repo,
   with `orca serve` running so the spec is served).

## Where to go next

- [argyle-labs/peacock](https://github.com/argyle-labs/peacock) — the dashboard
  source and its contributor guide.
- [`svelte-primer`](svelte-primer.md) — pointer to the Svelte 5 stack peacock uses.
- [`codebase-tour`](codebase-tour.md) — trace a request from the browser
  through orca's tool dispatch.
