<script lang="ts">
  import Button from '$lib/components/Button.svelte';
  import Spinner from '$lib/components/Spinner.svelte';
  import { notifications } from '$lib/stores/notifications';
  import { getLibraryDocs } from '$lib/api/client';
  import { marked } from 'marked';

  let q = $state('');
  let topic = $state('');
  let result: string = $state('');
  let loading = $state(false);

  async function lookup() {
    if (!q.trim() || loading) return;
    loading = true;
    try {
      const data = await getLibraryDocs({ q, topic: topic || undefined });
      result = (data as any)?.content ?? (data as any)?.text ?? JSON.stringify(data, null, 2);
    } catch (e) { notifications.error(String(e)); }
    finally { loading = false; }
  }
</script>

<svelte:head><title>Library Docs — orca</title></svelte:head>

<div class="page">
  <h1>Library Docs</h1>
  <div class="search-row">
    <input bind:value={q} placeholder="Package name (e.g. svelte, axum)" class="text-input" onkeydown={(e) => e.key === 'Enter' && lookup()} />
    <input bind:value={topic} placeholder="Topic (optional)" class="text-input topic" onkeydown={(e) => e.key === 'Enter' && lookup()} />
    <Button variant="primary" onclick={lookup} disabled={loading || !q.trim()}>
      {#if loading}<Spinner size={14} />{/if} Look up
    </Button>
  </div>
  {#if result}
    <div class="result">{@html marked(result)}</div>
  {/if}
</div>

<style>
  .search-row { display: flex; gap: var(--space-3); margin-bottom: var(--space-6); }
  .text-input { flex: 1; background: var(--color-surface); border: 1px solid var(--color-border); color: var(--color-text); padding: var(--space-2) var(--space-3); border-radius: var(--radius-sm); font-size: var(--text-sm); }
  .text-input:focus { outline: none; border-color: var(--color-accent); }
  .topic { flex: 0.4; }
  .result { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-md); padding: var(--space-6); line-height: 1.7; }
  .result :global(h1), .result :global(h2), .result :global(h3) { margin-top: var(--space-6); }
  .result :global(code) { background: var(--color-surface-2); padding: 1px 4px; border-radius: 3px; }
</style>
