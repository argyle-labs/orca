# Svelte 5 Primer

This primer teaches the Svelte 5 concepts you'll encounter in this codebase. Every example is drawn from actual files in the peacock web-UI plugin (repo [argyle-labs/peacock](https://github.com/argyle-labs/peacock), SvelteKit project at `peacock/ui/src/`). Read alongside the [official Svelte docs](https://svelte.dev/docs/svelte/overview) for deeper reference.

---

## What a Svelte component looks like

A `.svelte` file has three optional sections — script, template, style — always in that order:

```svelte
<!-- peacock/ui/src/lib/components/Spinner.svelte (simplified) -->
<script lang="ts">
  let { size = 16 }: { size?: number } = $props();
</script>

<div class="spinner" style="width:{size}px; height:{size}px"></div>

<style>
  .spinner { border-radius: 50%; border: 2px solid var(--color-border); /* ... */ }
</style>
```

`<script lang="ts">` is TypeScript. The template is HTML with a few extra features. `<style>` is CSS **automatically scoped to this component** — `.spinner` here only affects `.spinner` elements inside this file.

---

## Runes — Svelte 5's reactivity model

Svelte 5 introduces *runes*: special syntax that tells Svelte what to track and update. You'll see four of them constantly.

### `$props()` — declare inputs

Every component receives data through props. You declare them with `$props()`:

```svelte
<!-- TopNav.svelte (simplified) -->
<script lang="ts">
  let { onsearchopen, oncommandopen }: {
    onsearchopen: () => void;
    oncommandopen: () => void;
  } = $props();
</script>
```

`$props()` returns an object. Destructuring it names the individual props and their types. The caller passes them like HTML attributes:

```svelte
<TopNav onsearchopen={() => searchOpen = true} oncommandopen={() => commandOpen = true} />
```

**Default values** work in the destructure:

```svelte
let { size = 16 }: { size?: number } = $props();
```

`size` defaults to `16` if the caller doesn't provide it. The `?` in the type marks it optional.

---

### `$state()` — reactive local state

`$state()` declares a value that, when changed, triggers a re-render of anything that reads it:

```svelte
<!-- From +layout.svelte -->
<script lang="ts">
  let searchOpen    = $state(false);
  let commandOpen   = $state(false);
  let healthOpen    = $state(false);
</script>
```

Assign directly to update:

```svelte
<button onclick={() => searchOpen = true}>Search</button>
```

Svelte tracks the assignment and updates the DOM. There's no setter function — this is the key difference from React's `useState`.

For objects and arrays, mutation is tracked too:

```svelte
let treeData = $state<Record<string, TreeNode[]>>({});

// Later — Svelte notices this mutation
treeData = (data ?? {}) as Record<string, TreeNode[]>;
```

---

### `$derived` — computed values

`$derived` creates a value that automatically recalculates when its dependencies change:

```svelte
<!-- From +layout.svelte -->
const FULLSCREEN = ['/schema'];
const isFullscreen = $derived(
  FULLSCREEN.some(p => $page.url.pathname === p || $page.url.pathname.startsWith(p + '?'))
);
```

`isFullscreen` recalculates whenever `$page.url.pathname` changes. It's not a function — you read it like a variable. Svelte figures out the dependencies automatically by tracking what the expression reads.

For simple expressions use `$derived(expr)`. For complex logic with multiple steps:

```svelte
const result = $derived.by(() => {
  const filtered = items.filter(i => i.active);
  return filtered.map(i => i.name).join(', ');
});
```

---

### `$effect()` — side effects

`$effect()` runs after the DOM updates whenever its dependencies change — equivalent to React's `useEffect` with automatic dependency tracking:

```svelte
<!-- From +layout.svelte -->
$effect(() => { recordNav($page.url.pathname); });
```

Every time `$page.url.pathname` changes, `recordNav` is called with the new path. No dependency array needed — Svelte tracks what you read inside the effect.

Return a cleanup function for teardown:

```svelte
$effect(() => {
  const timer = setInterval(check, 1000);
  return () => clearInterval(timer);  // runs when effect re-fires or component unmounts
});
```

---

## Template syntax

### Interpolation

```svelte
<span>{entry.path}</span>
<span class="card-time">{timeAgo(entry.ts)}</span>
```

Curly braces `{}` evaluate any JavaScript expression.

### `{#if}` / `{:else}`

```svelte
{#if $serverHealth === 'down'}
  <div class="server-banner">Orca server is unreachable</div>
{/if}

{#if recent.length > 0}
  <!-- recent list -->
{:else}
  <!-- quick links -->
{/if}
```

### `{#each}`

```svelte
{#each QUICK_LINKS as link}
  <a href={link.href} class="card">{link.label}</a>
{/each}

<!-- With index -->
{#each items as item, i}
  <li>{i}: {item.name}</li>
{/each}
```

### `{@html}` — raw HTML output

Used when you have HTML as a string (e.g., after running markdown through a parser):

```svelte
<!-- From [...slug]/+page.svelte -->
<script lang="ts">
  import { marked } from 'marked';
  let { data } = $props();
  const html = $derived(data.content ? marked(data.content) : '');
</script>

<article class="doc">{@html html}</article>
```

`{@html}` bypasses escaping. Only use it with content you trust — never with user input directly.

### `{@render children()}` — slot content

Svelte 5 replaced the old `<slot>` system with *snippets*. A layout that wraps child content uses:

```svelte
<!-- +layout.svelte -->
<script lang="ts">
  let { children } = $props();
</script>

<main class="main-content">{@render children()}</main>
```

`children` is a snippet — callable with `{@render children()}`. Child pages render inside the `<main>`.

---

## Event handling

Events are props that start with `on`:

```svelte
<!-- Inline handler -->
<button onclick={() => collapsed = !collapsed}>Toggle</button>

<!-- Function reference -->
<button onclick={toggleCollapse}>Toggle</button>

<!-- Keyboard event on the window -->
<svelte:window onkeydown={handleKeydown} />
```

In Svelte 5, event handlers are just props (`onclick`, `onkeydown`, etc.) — no special `on:` directive syntax. The `svelte:window` element attaches listeners to the global window object.

---

## `onMount` — lifecycle hook

`onMount` runs once after the component first renders. Use it for browser-only code (DOM access, localStorage, fetch):

```svelte
<!-- From Sidebar.svelte -->
<script lang="ts">
  import { onMount } from 'svelte';

  onMount(async () => {
    collapsed = localStorage.getItem('sidebar-collapsed') === '1';

    try {
      const data = await getTree({});
      treeData = (data ?? {}) as Record<string, TreeNode[]>;
    } catch {}
  });
</script>
```

`onMount` only runs in the browser, never during server-side rendering. It can return a cleanup function (runs on unmount).

---

## Svelte stores — shared state

A *store* is an observable value that any component can subscribe to. The Svelte `writable` function creates one:

```typescript
// peacock/ui/src/lib/stores/serverHealth.ts
import { writable } from 'svelte/store';

function createServerHealth() {
  const { subscribe, set } = writable<ServerStatus>('unknown');

  async function check() {
    try {
      const res = await fetch('/api/health', { cache: 'no-store' });
      set(res.ok ? 'up' : 'down');     // update the store
    } catch {
      set('down');
    }
  }

  return { subscribe, start, retry };
}

export const serverHealth = createServerHealth();
```

Subscribe in a component by prefixing with `$`:

```svelte
<!-- The $ prefix auto-subscribes and unsubscribes -->
{#if $serverHealth === 'down'}
  <div class="server-banner">Orca server is unreachable</div>
{/if}
```

The `$serverHealth` syntax is Svelte's *auto-subscription*: Svelte subscribes when the component mounts and unsubscribes when it unmounts. No cleanup code needed.

**SvelteKit's built-in stores** follow the same pattern:

```svelte
<script lang="ts">
  import { page } from '$app/stores';
</script>

<!-- Read the current URL -->
<span>{$page.url.pathname}</span>
```

`$page` is a SvelteKit-provided store with the current page's URL, params, and route data.

---

## CSS in Svelte

### Scoped by default

Styles in `<style>` only apply to the current component. Svelte adds a unique attribute (like `svelte-xyz`) to elements and rewrites selectors to match. Two components can both have `.title` without collision.

```svelte
<style>
  /* Only affects .home inside THIS component */
  .home { padding: var(--space-8); max-width: 800px; }
</style>
```

### `:global()` — escape scoping

When you need to style child HTML (e.g., rendered markdown where you can't add Svelte attributes):

```svelte
<style>
  /* Targets ALL h1 elements inside .doc, regardless of where they came from */
  .doc :global(h1) { margin-top: var(--space-8); }
  .doc :global(code) { background: var(--color-surface-2); }
</style>
```

### CSS variables

The app uses CSS custom properties throughout — defined globally in `app.css`, read everywhere:

```svelte
<style>
  .card {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
  }
  .card:hover { border-color: var(--color-accent); }
</style>
```

These switch automatically with the theme. Never hard-code colors — always use a CSS variable.

---

## SvelteKit routing

SvelteKit uses **file-based routing** in `src/routes/`. The file name determines the URL.

```
src/routes/
  +layout.svelte        → wraps ALL pages
  +page.svelte          → renders at /
  schema/
    +page.svelte        → renders at /schema
  [...slug]/
    +page.svelte        → catches /anything/else
    +page.ts            → load function for data fetching
```

### Load functions (`+page.ts`)

Data fetching belongs in `+page.ts`, not in the component. The `load` function runs before the component renders and passes data as a prop:

```typescript
// peacock/ui/src/routes/[...slug]/+page.ts
import type { PageLoad } from './$types';
import { getDoc } from '$lib/api/client';

export const ssr = false;  // client-side only (no SSR)

export const load: PageLoad = async ({ params }) => {
  const slug = params.slug ?? '';
  const parts = slug.split('/').filter(Boolean);
  const root = parts[0] ?? 'orca';
  const path = parts.slice(1).join('/');

  if (!path) return { content: '', root, path: '' };

  const raw = await getDoc({ root, path });
  return { content: String(raw ?? ''), root, path };
};
```

The returned object becomes the `data` prop in the corresponding `+page.svelte`:

```svelte
<!-- [...slug]/+page.svelte -->
<script lang="ts">
  let { data } = $props();              // data = { content, root, path }
  const html = $derived(marked(data.content));
</script>
```

### Navigation

Use `<a href="...">` for all navigation — SvelteKit intercepts `<a>` clicks and handles routing without full page reloads. No special `<Link>` component needed.

```svelte
<a href="/schema">Schema Explorer</a>
<a href={entry.path}>{entry.path}</a>
```

### `$lib` alias

`$lib` resolves to `src/lib/`. Use it for cross-cutting imports so you don't write `../../../`:

```svelte
<script lang="ts">
  import { getTree } from '$lib/api/client';
  import TopNav from '$lib/components/TopNav.svelte';
  import { serverHealth } from '$lib/stores/serverHealth';
</script>
```

---

## Svelte vs React — quick comparison

| Concept | React | Svelte 5 |
|---------|-------|----------|
| State | `const [x, setX] = useState(v)` | `let x = $state(v)` |
| Update state | `setX(newVal)` | `x = newVal` |
| Computed | `useMemo(() => expr, [deps])` | `const c = $derived(expr)` |
| Side effect | `useEffect(() => {}, [deps])` | `$effect(() => {})` |
| Props | `function Comp({ foo }: Props)` | `let { foo } = $props()` |
| Slot content | `{children}` | `{@render children()}` |
| Loop | `{items.map(i => <li>{i}</li>)}` | `{#each items as i}<li>{i}</li>{/each}` |
| Conditional | `{cond && <Foo />}` | `{#if cond}<Foo />{/if}` |
| Raw HTML | `dangerouslySetInnerHTML` | `{@html str}` |
| Global event | `window.addEventListener(...)` | `<svelte:window onevent={fn} />` |

---

## Where to go next

- [`frontend-guide`](learn/frontend-guide) — add a new page or endpoint step by step
- [`codebase-tour`](learn/codebase-tour) — trace a real request through the full stack
- Official reference: [svelte.dev/docs](https://svelte.dev/docs/svelte/overview)
- SvelteKit reference: [svelte.dev/docs/kit](https://svelte.dev/docs/kit/introduction)
- `peacock/ui/src/routes/+layout.svelte` — the root component, all patterns in one file
- `peacock/ui/src/lib/stores/serverHealth.ts` — a custom store with polling
