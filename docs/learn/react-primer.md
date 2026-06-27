# React Primer

> **Note:** The orca frontend migrated from React to **Svelte 5 + SvelteKit** in 2026. This document is preserved as historical reference — the patterns here no longer apply to the current codebase.
>
> For the current frontend, read [`svelte-primer`](learn/svelte-primer) instead.

---

This primer covers the React 19 patterns that were used in the orca frontend before the Svelte migration. Examples came from real components that no longer exist in `projects/frontend/src/`.

---

## Components

A React component is a function that returns JSX. JSX looks like HTML but it's TypeScript:

```tsx
// From projects/frontend/src/components/MarkdownRenderer.tsx
export function MarkdownRenderer({ content }: { content: string }) {
  return (
    <article className="markdown">
      <ReactMarkdown remarkPlugins={[remarkGfm]}>
        {content}
      </ReactMarkdown>
    </article>
  );
}
```

The `{ content }: { content: string }` pattern is TypeScript destructuring with an inline type. `content` is the prop; `{ content: string }` is its type.

`export function` makes it importable by other files. Components start with a capital letter — lowercase tags (`div`, `span`) are HTML; capitalized names (`MarkdownRenderer`) are components.

---

## Props

Props are how you pass data into a component. They flow *down* — a parent passes props to a child; the child never mutates them.

```tsx
// Caller
<MarkdownRenderer content={myMarkdownString} />

// Callee — receives content as a prop
function MarkdownRenderer({ content }: { content: string }) { ... }
```

When the prop type gets complex, define it separately:

```tsx
interface TreeNode {
  name: string;
  path: string;
  type: 'file' | 'dir';
  children?: TreeNode[];  // ? means optional
}

function TreeNodeItem({ node, rootName, depth }: {
  node: TreeNode;
  rootName: string;
  depth: number;
}) { ... }
```

---

## State — `useState`

State is data owned by a component that, when changed, causes a re-render:

```tsx
// From DocPage.tsx
const [content, setContent] = useState<string | null>(null);
const [error, setError]     = useState(false);
```

`useState<string | null>(null)` — TypeScript generic says "this state holds a `string` or `null`", starting as `null`.

`setContent(...)` triggers a re-render with the new value. Never mutate the state variable directly — always call the setter.

The `[value, setter]` pattern is array destructuring. React returns a pair; you name them anything you want.

---

## Side effects — `useEffect`

`useEffect` runs *after* a render. Use it for fetching data, subscriptions, or DOM manipulation:

```tsx
// From DocPage.tsx
useEffect(() => {
  setContent(null);
  setError(false);

  fetch(`/api/doc?root=${root}&path=${docPath}`)
    .then((r) => (r.ok ? r.text() : Promise.reject()))
    .then((raw) => setContent(raw))
    .catch(() => setError(true));
}, [pathname]);  // dependency array
```

The second argument `[pathname]` is the *dependency array*. The effect re-runs whenever `pathname` changes. An empty array `[]` means "run once on mount." No array at all means "run after every render" — almost never what you want.

The function can return a cleanup: `return () => { cancelled = true; }` — runs before the effect fires again or when the component unmounts. Used in `Sidebar.tsx` to cancel in-flight fetches when the component unmounts.

---

## TanStack Query — server state

Manual `fetch` + `useState` + `useEffect` gets repetitive. TanStack Query replaces this pattern:

```tsx
// Hypothetical example matching the pattern used in this app
const { data, isLoading, error } = useQuery({
  queryKey: ['doc', root, path],
  queryFn: () => fetch(`/api/doc?root=${root}&path=${path}`).then(r => r.text()),
});
```

`queryKey` is the cache key — if the same key is requested from multiple components, only one fetch fires. The result is shared.

`isLoading` is `true` while the first fetch is in flight. `data` is `undefined` during that time.

TanStack Query handles:
- Deduplication of identical requests
- Background refetching when the window regains focus
- Stale time configuration
- Error retry

### Mutations

For POST/PUT/DELETE operations, use `useMutation`:

```tsx
const { mutate, isPending } = useMutation({
  mutationFn: (action: string) =>
    fetch('/api/docker/action', {
      method: 'POST',
      body: JSON.stringify({ service, action }),
    }),
  onSuccess: () => queryClient.invalidateQueries({ queryKey: ['services'] }),
});
```

`invalidateQueries` tells TanStack Query to refetch the `services` query — so the UI reflects the new state after the action completes.

---

## TanStack Router — navigation

Routes are defined in code (not file-based). The catch-all `$` route in this app handles all doc URLs:

```tsx
// Simplified from the route definition
const docRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '$',        // matches anything not matched by a named route
  component: DocPage,
});
```

Navigation uses the `<Link>` component (never `<a>` for internal links):

```tsx
// From Sidebar.tsx
<Link
  to={href}
  className="tree-file"
  activeProps={{ className: 'tree-file active' }}
>
  {node.name}
</Link>
```

`activeProps` applies those props when the link's URL matches the current location — built-in active state, no manual comparison.

Reading the current URL in a component:

```tsx
const { pathname } = useLocation();
```

---

## Mantine — UI components

Mantine provides styled components so you don't have to build buttons, menus, and modals from scratch.

```tsx
import { Menu, ActionIcon, Group } from '@mantine/core';

// From Sidebar.tsx
<Menu shadow="md" width={140} position="bottom-end">
  <Menu.Target>
    <ActionIcon variant="subtle" size="sm" color="gray">
      {/* icon SVG */}
    </ActionIcon>
  </Menu.Target>
  <Menu.Dropdown>
    {options.map((opt) => (
      <Menu.Item key={opt.id} onClick={() => select(opt)}>
        {opt.label}
      </Menu.Item>
    ))}
  </Menu.Dropdown>
</Menu>
```

Mantine uses compound components (`Menu.Target`, `Menu.Dropdown`, `Menu.Item`) — each sub-component plays a specific role in the parent.

Theming is CSS-variable-based — `var(--accent)`, `var(--surface)`, etc. These are set in `index.css` based on the `data-theme` and `data-mode` attributes on `<html>`.

---

## Context — shared state across the tree

Context lets you pass data through the component tree without threading props manually:

```tsx
// From ThemeContext.tsx
const ThemeContext = createContext<ThemeContextValue | null>(null);

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [theme, setTheme] = useState<ThemeName>('violet');
  // ...
  return (
    <ThemeContext.Provider value={{ theme, mode, setTheme, toggleMode }}>
      {children}
    </ThemeContext.Provider>
  );
}

export function useAppTheme() {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error('useAppTheme must be used within ThemeProvider');
  return ctx;
}
```

The `Provider` wraps the app at the root level. Any component inside can call `useAppTheme()` to read or change the theme — no props needed.

This pattern (context + custom hook) is the standard way to share global state in this codebase.

---

## TypeScript with React

### Generic components

```tsx
// A component that works with any item type
function List<T>({ items, render }: { items: T[]; render: (item: T) => React.ReactNode }) {
  return <ul>{items.map((item, i) => <li key={i}>{render(item)}</li>)}</ul>;
}
```

### Event handlers

```tsx
function handleClick(e: React.MouseEvent<HTMLAnchorElement>) {
  e.preventDefault();
  // ...
}
```

React exports the event types. `MouseEvent<HTMLAnchorElement>` means a mouse event on an anchor element.

### Conditional rendering

```tsx
if (error) return <p className="muted">Document not found.</p>;
if (!content) return <p className="muted">Loading…</p>;
return <MarkdownRenderer content={content} />;
```

Early returns keep the happy path clean. React renders whatever the function returns — including `null` to render nothing.

---

## Component lifecycle, summarized

```
mount → render → useEffect fires
               ↓
       state/prop change → re-render → useEffect fires (if deps changed)
               ↓
       unmount → useEffect cleanup runs
```

Most bugs in React come from:
1. Missing dependencies in the `useEffect` array (stale closure — the effect sees an old value)
2. Setting state after unmount (the cleanup pattern fixes this)
3. Forgetting that renders happen multiple times — effects, not renders, are where you fetch

---

## Where to go next

- [`frontend-guide`](learn/frontend-guide) — how to add a new page or API endpoint
- `projects/frontend/src/routes/DocPage.tsx` — a complete, simple example of fetch + render
- `projects/frontend/src/components/Sidebar.tsx` — useEffect, localStorage, and compound state
- `projects/frontend/src/contexts/ThemeContext.tsx` — the context pattern in full
