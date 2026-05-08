<script lang="ts">
  import { browser } from '$app/environment';

  let { data } = $props();
  const spec: any = $derived(data.spec);
  const repo: string = $derived(data.repo);

  function hasOpenApi(s: any): boolean {
    return s?.files?.full != null && s?.files?.full !== false;
  }
  function hasGraphql(s: any): boolean {
    return !!s?.hasGraphql;
  }

  // Default view: REST if available, else GraphQL
  let viewMode = $state<'rest' | 'graphql'>('rest');

  $effect(() => {
    const s = spec;
    viewMode = hasOpenApi(s) ? 'rest' : 'graphql';
  });

  const iframeSrc = $derived(
    spec && hasOpenApi(spec) && viewMode === 'rest'
      ? `/scalar?url=${encodeURIComponent(`/api/specs/${repo}`)}`
      : ''
  );

  // GraphiQL mounting
  let gqlContainer: HTMLDivElement | undefined = $state();
  let graphiqlInstance: any = null;

  $effect(() => {
    const container = gqlContainer;
    const r = repo;
    if (!browser || !container || !hasGraphql(spec) || viewMode !== 'graphql') return;

    let cancelled = false;
    import('$lib/graphiql-mount').then(({ createGraphiQL }) => {
      if (cancelled || !container) return;
      if (graphiqlInstance) { graphiqlInstance.unmount(); graphiqlInstance = null; }
      createGraphiQL(container, r).then(inst => {
        if (cancelled) { inst.unmount(); return; }
        graphiqlInstance = inst;
      });
    });

    return () => {
      cancelled = true;
    };
  });

  // Generating state for specs with neither REST nor GQL yet
  let generating = $state<string | null>(null);

  $effect(() => {
    const s = spec;
    const r = repo;
    generating = null;
    if (!s || hasOpenApi(s) || hasGraphql(s)) return;
    generatePoll(r);
  });

  async function generatePoll(r: string) {
    generating = r;
    for (let i = 0; i < 20; i++) {
      await new Promise(res => setTimeout(res, 3000));
      try {
        const res = await fetch(`/api/specs/${r}`, { headers: { Accept: 'application/json' } });
        if (res.ok) { generating = null; return; }
        if (res.status !== 202) break;
      } catch { break; }
    }
    generating = null;
  }

  const showTabs = $derived(spec && hasOpenApi(spec) && hasGraphql(spec));
</script>

<div class="page-root">
  {#if spec}
    {#if showTabs}
      <div class="tabs">
        <button class="tab" class:active={viewMode === 'rest'} onclick={() => viewMode = 'rest'}>REST</button>
        <button class="tab" class:active={viewMode === 'graphql'} onclick={() => viewMode = 'graphql'}>GraphQL</button>
      </div>
    {/if}

    {#if hasOpenApi(spec) && viewMode === 'rest'}
      <iframe src={iframeSrc} title="API Reference" class="scalar-frame"></iframe>

    {:else if hasGraphql(spec) && viewMode === 'graphql'}
      <div class="gql-wrap" bind:this={gqlContainer}></div>

    {:else if generating === repo}
      <div class="info"><p>Generating spec for <code>{repo}</code>… retrying every 3s</p></div>

    {:else}
      <div class="info"><p>No spec available for <code>{repo}</code>.</p></div>
    {/if}

  {:else}
    <div class="info"><p>Spec not found: <code>{repo}</code>.</p></div>
  {/if}
</div>

<style>
  .page-root {
    display: flex; flex-direction: column;
    flex: 1; min-height: 0; overflow: hidden;
  }

  .tabs {
    display: flex;
    gap: 4px;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-border);
    flex-shrink: 0;
  }
  .tab {
    background: none;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    color: var(--color-text-dim);
    cursor: pointer;
    font-size: var(--text-xs);
    font-weight: var(--weight-medium);
    padding: 3px var(--space-3);
    transition: color var(--transition-fast), background var(--transition-fast);
  }
  .tab:hover { color: var(--color-text); background: var(--color-surface-2); }
  .tab.active { color: var(--color-accent); background: rgba(124,106,247,0.12); border-color: rgba(124,106,247,0.25); }

  .scalar-frame { width: 100%; height: 100%; border: none; display: block; }

  .gql-wrap {
    flex: 1;
    height: 100%;
    overflow: hidden;
  }
  .gql-wrap :global(.graphiql-container) { height: 100%; }

  .info {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--color-text-dim);
    font-size: var(--text-sm);
  }
  .info code {
    background: var(--color-surface-2);
    padding: 2px 6px;
    border-radius: 3px;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
</style>
