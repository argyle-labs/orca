# Frontend Guide

How to make changes to the orca web UI. Covers adding pages, API endpoints, and working with the generated API client. The frontend is a **SvelteKit 2 + Svelte 5** app compiled into the Rust binary.

---

## Development setup

```bash
make dev
```

This starts two processes in parallel:
- **cargo-watch** — rebuilds and reinstalls the Rust binary on every `.rs` file save
- **Vite HMR** — serves the frontend at `:12001` with instant hot module replacement

The Vite config proxies `/api/*` to `:12000`, so API calls in the browser reach the Rust server automatically. In dev, browse to `http://localhost:12001`.

---

## Project layout

```
projects/frontend/src/
  routes/               ← SvelteKit file-based routing
    +layout.svelte      ← wraps every page (nav, search, notifications)
    +layout.ts          ← layout load function
    +page.svelte        ← home page ( / )
    [...slug]/
      +page.ts          ← load function: fetches doc content
      +page.svelte      ← doc viewer (renders markdown)
    schema/+page.svelte
    session/+page.svelte
    system/+page.svelte
    mcps/+page.svelte
    ...
  lib/
    api/
      client.ts         ← generated API client (never edit)
      types.ts          ← generated TypeScript types (never edit)
    components/         ← shared Svelte components
      Sidebar.svelte, TopNav.svelte, SearchModal.svelte, ...
    stores/
      serverHealth.ts   ← custom store: polls /api/health
      navHistory.ts     ← stores recent navigation
  app.css               ← global CSS variables + resets
  app.html              ← HTML shell (Vite entry point)
```

---

## Adding a new page

### 1. Create the route file

SvelteKit maps filenames to URLs. Create a directory and a `+page.svelte` file:

```
projects/frontend/src/routes/widgets/+page.svelte
```

```svelte
<script lang="ts">
  import { onMount } from 'svelte';

  let items = $state<{ id: number; name: string }[]>([]);
  let loading = $state(true);

  onMount(async () => {
    const res = await fetch('/api/widgets');
    items = await res.json();
    loading = false;
  });
</script>

<svelte:head><title>Widgets — orca</title></svelte:head>

<div class="page-content">
  <h1>Widgets</h1>

  {#if loading}
    <p class="dim">Loading…</p>
  {:else}
    <ul>
      {#each items as item}
        <li>{item.name}</li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .page-content { padding: var(--space-8); max-width: 800px; }
  .dim { color: var(--color-text-dim); }
</style>
```

That's it — SvelteKit finds the file and serves it at `/widgets`. No registration needed.

### 2. Use a load function for data fetching (preferred)

For cleaner separation, move fetching into a `+page.ts` alongside the component:

```typescript
// projects/frontend/src/routes/widgets/+page.ts
import type { PageLoad } from './$types';

export const ssr = false;  // client-side only

export const load: PageLoad = async () => {
  const res = await fetch('/api/widgets');
  const items = await res.json();
  return { items };
};
```

The component receives `data` automatically:

```svelte
<!-- projects/frontend/src/routes/widgets/+page.svelte -->
<script lang="ts">
  let { data } = $props();
  // data.items is fully available — no loading state needed
</script>

{#each data.items as item}
  <li>{item.name}</li>
{/each}
```

Load functions run before render, so the component never sees an intermediate loading state.

### 3. Add a nav link (optional)

Open `projects/frontend/src/lib/components/Sidebar.svelte` and find the navigation links section. Add:

```svelte
<a href="/widgets" class="nav-link" class:active={$page.url.pathname === '/widgets'}>
  Widgets
</a>
```

`class:active={condition}` is Svelte's conditional class shorthand — adds the `active` class when the condition is true.

---

## Adding a new API endpoint (Rust)

### 1. Create or open the handler file

Each domain has its own file in `projects/server/src/serve/api/`. Add a handler:

```rust
// projects/server/src/serve/api/widgets.rs
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct Widget {
    pub id: u32,
    pub name: String,
}

/// List all widgets
#[utoipa::path(
    get,
    path = "/api/widgets",
    tag = "widgets",
    responses(
        (status = 200, description = "Widget list", body = Vec<Widget>)
    )
)]
pub async fn get_widgets() -> impl IntoResponse {
    Json(vec![
        Widget { id: 1, name: "foo".into() },
    ])
}
```

The `#[utoipa::path]` attribute auto-generates the OpenAPI spec entry. Always include it.

### 2. Export from the module

Add to `projects/server/src/serve/api/mod.rs`:

```rust
pub mod widgets;
pub use widgets::*;
```

### 3. Register the route

In `projects/server/src/serve/openapi.rs`, add to the router:

```rust
.routes(routes!(api::get_widgets))
```

And register the schema:

```rust
#[openapi(
    components(schemas(
        // ... existing schemas ...
        api::Widget,
    ))
)]
```

### 4. Regenerate the TypeScript client

The API client is auto-generated from the OpenAPI spec. After changing the Rust API:

```bash
orca serve &      # must be running
cd projects/frontend
npm run gen        # regenerates src/lib/api/client.ts and types.ts
```

---

## Using the generated API client

`projects/frontend/src/lib/api/client.ts` contains one typed function per endpoint, generated from the OpenAPI spec. Use these instead of raw `fetch()`:

```svelte
<script lang="ts">
  import { getWidgets } from '$lib/api/client';
  import type { Widget } from '$lib/api/types';

  let widgets = $state<Widget[]>([]);

  onMount(async () => {
    widgets = await getWidgets() ?? [];
  });
</script>
```

The function signature, parameters, and return type are all inferred from the spec. TypeScript will catch mismatches at compile time.

**Never edit `client.ts` or `types.ts` directly** — they're overwritten on the next `npm run gen`.

---

## Svelte stores for shared state

When multiple components need the same data, use a store instead of prop-drilling.

### Using an existing store

```svelte
<script lang="ts">
  import { serverHealth } from '$lib/stores/serverHealth';
</script>

<!-- $serverHealth auto-subscribes; updates when value changes -->
{#if $serverHealth === 'down'}
  <span class="error">Offline</span>
{/if}
```

### Creating a new store

```typescript
// projects/frontend/src/lib/stores/myStore.ts
import { writable } from 'svelte/store';

const { subscribe, set, update } = writable<string[]>([]);

export const myList = {
  subscribe,                                   // required — makes $ syntax work
  add: (item: string) => update(list => [...list, item]),
  reset: () => set([]),
};
```

Any component can now import and use `$myList`.

---

## CSS conventions

All styles use CSS custom properties defined in `app.css`. Never hard-code colors or spacing.

**Color tokens:**
```css
var(--color-text)        /* primary text */
var(--color-text-dim)    /* secondary / muted text */
var(--color-surface)     /* card / panel backgrounds */
var(--color-surface-2)   /* inset surfaces, code backgrounds */
var(--color-border)      /* borders, dividers */
var(--color-accent)      /* interactive highlights */
var(--color-error)       /* error states */
```

**Spacing tokens:** `var(--space-1)` through `var(--space-8)` (4px scale).

**Typography:** `var(--text-xs)`, `var(--text-sm)`, `var(--text-base)`, `var(--text-lg)`.

**Example component style:**

```svelte
<style>
  .card {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    padding: var(--space-3) var(--space-4);
  }
  .card:hover { border-color: var(--color-accent); }
  .label { font-size: var(--text-xs); color: var(--color-text-dim); }
</style>
```

---

## Adding a doc to the learning system

Drop a `.md` file into `docs/` (or a subdirectory):

```
docs/learn/my-topic.md
```

It appears in the sidebar under **Docs → learn** after rebuilding the binary. `rust-embed` picks up all `.md` files at compile time — no registration needed.

For immediate availability without rebuilding, place it in the orca vault at `~/.orca/` — those docs are served live from the filesystem.

---

## Running tests

```bash
make test        # vitest (frontend) + cargo test (Rust)
make test-e2e    # Playwright end-to-end tests
```

Frontend unit tests use Vitest with `@testing-library/svelte`:

```typescript
// src/lib/components/Button.test.ts
import { render } from '@testing-library/svelte';
import Button from './Button.svelte';

test('renders label', () => {
  const { getByText } = render(Button, { props: { label: 'Click me' } });
  expect(getByText('Click me')).toBeTruthy();
});
```

---

## Building for release

```bash
make build
```

1. `npm run build` → generates `projects/frontend/dist/`
2. `cargo build --release` → embeds `dist/` into the binary via `rust-embed`

The released binary contains the complete SvelteKit app. No Node, no Vite, no separate web process needed at the install target. `orca serve` on the target machine serves everything.

---

## Type checking

```bash
cd projects/frontend
npm run check    # runs svelte-check — TypeScript + Svelte template type errors
```

Run this before committing frontend changes. It catches template type errors that `tsc` alone won't find (e.g., passing the wrong prop type to a Svelte component).
