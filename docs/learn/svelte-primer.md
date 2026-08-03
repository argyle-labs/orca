# Svelte 5 Primer

Pointer to the frontend stack. The orca web dashboard is the **`peacock`
plugin** — a SvelteKit 2 + Svelte 5 app in the separate
[argyle-labs/peacock](https://github.com/argyle-labs/peacock) repo.

Svelte components, runes (`$state`, `$derived`, `$effect`, `$props`), stores,
and SvelteKit routing are peacock concerns and are documented in that repo
alongside its source. Read them there, next to the code they describe:

- [argyle-labs/peacock](https://github.com/argyle-labs/peacock) — the dashboard
  source and contributor guide.
- Official reference: [svelte.dev/docs](https://svelte.dev/docs/svelte/overview)
  and [svelte.dev/docs/kit](https://svelte.dev/docs/kit/introduction).

## The orca boundary

orca serves peacock by proxying `/` to the plugin's `peacock.render` tool, and
peacock calls back through a typed client generated from orca's `#[orca_tool]`
OpenAPI spec. To expose a new endpoint the dashboard can call, see
[`frontend-guide`](learn/frontend-guide).
