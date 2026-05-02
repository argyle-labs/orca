# Frontend

The brain site is a **SvelteKit 2 + Svelte 5** single-page application compiled into the Rust binary at build time and served at `:12000`.

## Architecture overview

```
+layout.svelte         ← root layout: TopNav + Sidebar + route outlet
  +page.svelte         ← home page ( / )
  schema/+page.svelte
  session/+page.svelte
  mcps/+page.svelte
  system/+page.svelte
  settings/+page.svelte
  [...slug]/
    +page.ts           ← load function: fetches doc content from API
    +page.svelte       ← doc viewer: renders markdown
```

SvelteKit maps directory structure to URLs. Every `+page.svelte` is a route. `+layout.svelte` wraps all routes with the shared shell. The `[...slug]` catch-all route handles any path not matched by a named route — it interprets the path as `root/docpath` and renders the doc.

## Routing

**File-based** — the filesystem is the router. Creating `src/routes/widgets/+page.svelte` creates the route `/widgets`. No configuration or registration.

`export const ssr = false` in `+page.ts` files disables server-side rendering — the app runs entirely in the browser. The SvelteKit static adapter generates a pure client-side bundle.

## State management

State lives in three places:

1. **`$state()` rune** — reactive local state inside a component. Reassignment triggers a re-render. No setter function needed.
2. **Svelte stores** (`src/lib/stores/`) — observable values shared across components. Subscribe with the `$` prefix.
   - `serverHealth` — polls `/api/health`, tracks `'unknown' | 'up' | 'down'`
   - `navHistory` — records visited paths for the home page's "recently accessed" panel
3. **URL** — the source of truth for navigation state. `$page.url.pathname` from `$app/stores` replaces a global router context.

## Data fetching

All API calls go through the generated client in `src/lib/api/client.ts`. This file is produced by `npm run gen` from the OpenAPI spec at `/api/openapi.json`:

```svelte
<script lang="ts">
  import { getTree } from '$lib/api/client';
  import type { TreeNode } from '$lib/api/types';

  let treeData = $state<Record<string, TreeNode[]>>({});

  onMount(async () => {
    const data = await getTree({});
    treeData = (data ?? {}) as Record<string, TreeNode[]>;
  });
</script>
```

**Never call `fetch()` directly.** If a new API endpoint is added, run `npm run gen` to regenerate the client. This keeps the TypeScript types in sync with the Rust server.

For page-level data, use a `+page.ts` load function instead of `onMount`:

```typescript
// src/routes/[...slug]/+page.ts
export const load: PageLoad = async ({ params }) => {
  const raw = await getDoc({ root, path });
  return { content: String(raw ?? ''), root, path };
};
```

The returned object is available as `data` in the matching `+page.svelte`.

## Components

All shared components live in `src/lib/components/`. Every file is a `.svelte` single-file component.

| Component | Purpose |
|-----------|---------|
| `TopNav.svelte` | App header, triggers search and command palette |
| `Sidebar.svelte` | Doc tree navigation, collapsible, loads from `/api/tree` |
| `SearchModal.svelte` | Full-screen search (⌘K) |
| `CommandPalette.svelte` | Quick navigation (⌘/) |
| `HealthDashboard.svelte` | Rebuy local service health checks |
| `ServicesPanel.svelte` | Docker Compose service status cards |
| `DataTable.svelte` | Sortable/filterable table component |
| `Modal.svelte` / `ModalShell.svelte` | Dialog/overlay primitives |
| `Button.svelte` | Styled button with variants |
| `Badge.svelte` | Status badges |
| `Spinner.svelte` | Loading indicator |
| `StatusDot.svelte` | Colored status dot |
| `Notification.svelte` | Toast notification system |
| `Popover.svelte` | Floating contextual info |
| `PropertiesPanel.svelte` | Key-value property display |

## Routes

| Route | File | Purpose |
|-------|------|---------|
| `/` | `+page.svelte` | Dashboard: recently accessed + quick links |
| `/schema` | `schema/+page.svelte` | MySQL schema browser (full-screen) |
| `/session` | `session/+page.svelte` | Agent session runner |
| `/mcps` | `mcps/+page.svelte` | MCP server registry |
| `/system` | `system/+page.svelte` | Brain installation status |
| `/settings` | `settings/+page.svelte` | App configuration |
| `/api-docs` | `api-docs/+page.svelte` | Interactive OpenAPI reference (Scalar) |
| `/graphql` | `graphql/+page.svelte` | GraphQL IDE |
| `/ctx7` | `ctx7/+page.svelte` | Library documentation search |
| `/jira` | `jira/+page.svelte` | Jira issue browser |
| `/confluence` | `confluence/+page.svelte` | Confluence search |
| `/bitbucket` | `bitbucket/+page.svelte` | Repo/PR browser |
| `/*` (catch-all) | `[...slug]/+page.svelte` | Vault doc viewer |

## Theming

CSS custom properties in `app.css`. Theme colors switch by changing CSS variable values — no class toggling on individual elements.

Key variables: `--color-text`, `--color-text-dim`, `--color-surface`, `--color-surface-2`, `--color-border`, `--color-accent`, `--color-error`.

Spacing: `--space-1` through `--space-8` (4px scale).

## Build pipeline

```
npm run build
  └── vite build     ← Rolldown (Rust-based bundler), static SvelteKit build
      └── dist/      ← static files

cargo build --release
  └── rust-embed     ← compiles dist/ into binary
```

Vite 8 uses Rolldown for faster builds than webpack/Rollup. The output is a static bundle (HTML + JS + CSS) served directly from the Rust binary — no Node runtime needed at the install target.

## Dev workflow

```sh
make dev
```

Starts two processes:
1. `cargo watch` — rebuilds and reinstalls the Rust binary on Rust file changes
2. `vite dev` — HMR dev server on `:12001`, proxies `/api/` to `:12000`

In dev, Vite handles the frontend with instant hot reloads. The Rust binary handles only API routes. The site is **not** embedded in the debug binary — only `cargo build --release` embeds it.

## Generated files

`src/lib/api/client.ts` and `src/lib/api/types.ts` are generated by `npm run gen`. Do not edit them. After any backend API change:

```sh
# Server must be running
npm run gen   # (from projects/frontend/)
```
