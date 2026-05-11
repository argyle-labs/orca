<script lang="ts">
  import Button from '$lib/components/Button.svelte';
  import Spinner from '$lib/components/Spinner.svelte';
  import WorkspaceSetup from '$lib/components/WorkspaceSetup.svelte';
  import { searchConfluence, setPluginData } from '$lib/api/client';
  import { notifications } from '$lib/stores/notifications';
  import { invalidateAll } from '$app/navigation';

  let { data } = $props();
  let config: any = $derived(data.config);

  let cql = $state('');
  let results: any[] = $state([]);
  let loading = $state(false);

  async function search() {
    if (loading) return;
    loading = true;
    try {
      const spaceCql = config?.space ? `space = "${config.space}"` : '';
      const combined = [spaceCql, cql].filter(Boolean).join(' AND ') || undefined;
      const res = await searchConfluence({ cql: combined, limit: 25 });
      results = (res as any)?.results ?? [];
    } catch (e) { notifications.error(String(e)); }
    finally { loading = false; }
  }

  async function saveConfig(values: Record<string, string>) {
    await setPluginData({ id: 'rebuy', key: 'confluence_config', body: { value: { space: values.space } } });
    await invalidateAll();
  }
</script>

<svelte:head><title>Confluence — orca</title></svelte:head>

<div class="page">
  {#if !config}
    <WorkspaceSetup
      service="Confluence"
      fields={[
        { key: 'space', label: 'Default Space Key', placeholder: 'MYSPACE', hint: 'Leave blank to search all spaces', required: false },
      ]}
      onSave={saveConfig}
    />
  {:else}
    <div class="header">
      <h1>Confluence {#if config.space}<span class="space-badge">{config.space}</span>{/if}</h1>
      <button class="reconfigure" onclick={() => setPluginData({ id: 'rebuy', key: 'confluence_config', body: { value: {} } }).then(() => invalidateAll())}>
        Reconfigure
      </button>
    </div>
    <div class="search-bar">
      <input bind:value={cql} placeholder="CQL query (blank = recent pages)" class="text-input"
             onkeydown={(e) => e.key === 'Enter' && search()} />
      <Button variant="primary" onclick={search} disabled={loading}>
        {#if loading}<Spinner size={14} />{/if} Search
      </Button>
    </div>
    {#if results.length > 0}
      <table class="data-table">
        <thead><tr><th>Title</th><th>Space</th><th>Updated</th></tr></thead>
        <tbody>
          {#each results as r}
            <tr>
              <td><a href={r._links?.webui ?? '#'} target="_blank">{r.title}</a></td>
              <td>{r.space?.name ?? '—'}</td>
              <td>{r.lastModified?.slice(0,10) ?? '—'}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  {/if}
</div>

<style>
  .header { display: flex; align-items: center; gap: var(--space-3); margin-bottom: var(--space-4); }
  .header h1 { margin: 0; }
  .space-badge { font-size: var(--text-sm); font-weight: normal; color: var(--color-text-dim); background: var(--color-surface-2); padding: 2px 8px; border-radius: var(--radius-sm); font-family: var(--font-mono); }
  .reconfigure { background: none; border: none; cursor: pointer; font-size: var(--text-xs); color: var(--color-text-dim); padding: 2px 6px; border-radius: var(--radius-sm); margin-left: auto; }
  .reconfigure:hover { color: var(--color-text); background: var(--color-surface-2); }
  .search-bar { display: flex; gap: var(--space-3); margin-bottom: var(--space-6); }
  .text-input {
    flex: 1; background: var(--color-surface); border: 1px solid var(--color-border);
    color: var(--color-text); padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm); font-size: var(--text-sm); outline: none;
  }
  .text-input:focus { border-color: var(--color-accent); }
  .data-table { width: 100%; border-collapse: collapse; font-size: var(--text-sm); }
  .data-table th, .data-table td { padding: var(--space-2) var(--space-3); border-bottom: 1px solid var(--color-border); text-align: left; }
  .data-table th { color: var(--color-text-dim); font-weight: 500; }
</style>
