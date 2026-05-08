<script lang="ts">
  import { browser } from '$app/environment';
  import { page } from '$app/stores';
  import { listSpecs } from '$lib/api/client';
  import { onMount } from 'svelte';

  let repo = $state($page.url.searchParams.get('repo') ?? 'shopify-admin');
  let specs: any[] = $state([]);
  let container: HTMLDivElement;
  let graphiqlInstance: any = null;

  const gqlRepos = $derived(specs.filter((s: any) => !!s.hasGraphql));

  onMount(async () => {
    try {
      const result = await listSpecs();
      specs = (result as any[]) ?? [];
      // default to first graphql repo if param not in list
      if (!specs.find((s: any) => s.repo === repo && s.hasGraphql)) {
        repo = specs.find((s: any) => s.hasGraphql)?.repo ?? repo;
      }
    } catch {}

    await mountGraphiQL(repo);
  });

  async function mountGraphiQL(r: string) {
    if (!browser || !container) return;

    const { createGraphiQL } = await import('$lib/graphiql-mount');
    if (graphiqlInstance) {
      graphiqlInstance.unmount();
    }
    graphiqlInstance = await createGraphiQL(container, r);
  }

  async function switchRepo(r: string) {
    repo = r;
    await mountGraphiQL(r);
  }
</script>

<svelte:head><title>GraphQL — orca</title></svelte:head>

<div class="layout">
  <aside class="repo-list">
    <div class="repo-header">GraphQL</div>
    {#each gqlRepos as spec (spec.repo)}
      <button
        class="repo-btn"
        class:active={repo === spec.repo}
        onclick={() => switchRepo(spec.repo)}
      >
        {spec.repo}
      </button>
    {/each}
  </aside>

  <div class="ide-container" bind:this={container}></div>
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: 180px 1fr;
    height: calc(100vh - var(--nav-height));
    overflow: hidden;
  }

  .repo-list {
    border-right: 1px solid var(--color-border);
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow-y: auto;
  }

  .repo-header {
    font-size: var(--text-xs);
    font-weight: var(--weight-semibold);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-dim);
    padding: var(--space-1) var(--space-2);
    margin-bottom: var(--space-2);
  }

  .repo-btn {
    background: none;
    border: none;
    border-radius: var(--radius-sm);
    color: var(--color-text-dim);
    cursor: pointer;
    font-size: var(--text-xs);
    font-family: var(--font-mono);
    padding: var(--space-2) var(--space-3);
    text-align: left;
  }
  .repo-btn:hover { background: var(--color-surface-2); color: var(--color-text); }
  .repo-btn.active { background: var(--color-surface-2); color: var(--color-text); border-left: 2px solid #e746a0; }

  .ide-container {
    height: 100%;
    overflow: hidden;
  }

  /* Override GraphiQL defaults to fit our dark theme */
  .ide-container :global(.graphiql-container) {
    height: 100%;
    font-family: var(--font-mono);
  }

  .ide-container :global(.graphiql-container),
  .ide-container :global(.CodeMirror) {
    background: var(--color-bg);
    color: var(--color-text);
  }
</style>
