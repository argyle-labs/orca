<script lang="ts">
  import { act } from '$lib/stores/notifications';
  import { listPluginCreds, setPluginCred, deletePluginCred, syncPluginCreds } from '$lib/api/client';
  import type { PluginInfo, CredInfo } from '$lib/api/types';

  let { data } = $props();
  let plugins: PluginInfo[] = $derived(data.plugins ?? []);

  // ── Workspaces ───────────────────────────────────────────────────────────────
  const WORKSPACE_LABELS: Record<string, string> = {
    homelab:  'Homelab',
    personal: 'Personal',
    work:     'Rebuy',
  };

  let workspaces = $derived(
    [...new Set(plugins.map(p => p.tier))].sort((a, b) => {
      const order = ['homelab', 'work', 'personal'];
      return (order.indexOf(a) ?? 99) - (order.indexOf(b) ?? 99);
    })
  );

  let activeWorkspace = $state('');
  $effect(() => { if (workspaces.length && !activeWorkspace) activeWorkspace = workspaces[0]; });

  let visiblePlugins = $derived(plugins.filter(p => p.tier === activeWorkspace));

  // ── Credentials panel ────────────────────────────────────────────────────────
  let selectedPlugin = $state<PluginInfo | null>(null);
  let creds = $state<CredInfo[]>([]);
  let loadingCreds = $state(false);
  let newKey = $state('');
  let newValue = $state('');
  let adding = $state(false);

  async function selectPlugin(p: PluginInfo) {
    selectedPlugin = p;
    loadingCreds = true;
    creds = (await act(() => listPluginCreds({ id: p.id })) as CredInfo[]) ?? [];
    loadingCreds = false;
  }

  async function addCred() {
    if (!selectedPlugin || !newKey.trim() || !newValue.trim()) return;
    adding = true;
    await act(
      () => setPluginCred({ id: selectedPlugin!.id, body: { key: newKey.trim(), value: newValue.trim() } }),
      { success: `Stored ${newKey}` },
    );
    newKey = '';
    newValue = '';
    creds = (await listPluginCreds({ id: selectedPlugin.id })) as CredInfo[];
    adding = false;
  }

  async function removeCred(key: string) {
    if (!selectedPlugin) return;
    await act(
      () => deletePluginCred({ id: selectedPlugin!.id, key }),
      { success: `Removed ${key}` },
    );
    creds = (await listPluginCreds({ id: selectedPlugin.id })) as CredInfo[];
  }

  async function syncCreds() {
    if (!selectedPlugin) return;
    await act(
      () => syncPluginCreds({ id: selectedPlugin!.id }),
      { success: `Synced credentials to ${selectedPlugin.id}` },
    );
    creds = (await listPluginCreds({ id: selectedPlugin.id })) as CredInfo[];
  }
</script>

<svelte:head><title>Plugins — orca</title></svelte:head>

<div class="page">
  <h1>Plugins</h1>

  <!-- Workspace tabs -->
  {#if workspaces.length > 1}
    <div class="tabs">
      {#each workspaces as ws}
        <button
          class="tab"
          class:active={ws === activeWorkspace}
          onclick={() => { activeWorkspace = ws; selectedPlugin = null; creds = []; }}
        >
          {WORKSPACE_LABELS[ws] ?? ws}
        </button>
      {/each}
    </div>
  {/if}

  <div class="layout">
    <!-- Plugin list -->
    <div class="plugin-list">
      {#each visiblePlugins as p}
        <button
          class="plugin-card"
          class:selected={selectedPlugin?.id === p.id}
          onclick={() => selectPlugin(p)}
        >
          <span class="plugin-id">{p.id}</span>
          <span class="plugin-endpoint">{p.mcpCommand ?? 'stdio'}</span>
          <span class="badge" class:enabled={p.enabled} class:disabled={!p.enabled}>
            {p.enabled ? 'enabled' : 'disabled'}
          </span>
        </button>
      {:else}
        <p class="empty">No plugins in this workspace.</p>
      {/each}
    </div>

    <!-- Credentials panel -->
    {#if selectedPlugin}
      <div class="creds-panel">
        <div class="creds-header">
          <h2>{selectedPlugin.id}</h2>
          <button class="btn-sync" onclick={syncCreds}>Sync to plugin</button>
        </div>

        {#if loadingCreds}
          <p class="loading">Loading…</p>
        {:else}
          <table class="creds-table">
            <thead>
              <tr><th>Key</th><th>Synced</th><th>Updated</th><th></th></tr>
            </thead>
            <tbody>
              {#each creds as c}
                <tr>
                  <td class="key">{c.key}</td>
                  <td><span class="sync-badge" class:synced={c.synced}>{c.synced ? 'synced' : 'pending'}</span></td>
                  <td class="updated">{c.updatedAt}</td>
                  <td><button class="btn-remove" onclick={() => removeCred(c.key)}>×</button></td>
                </tr>
              {:else}
                <tr><td colspan="4" class="empty-row">No credentials stored.</td></tr>
              {/each}
            </tbody>
          </table>

          <form class="add-cred" onsubmit={(e) => { e.preventDefault(); addCred(); }}>
            <input bind:value={newKey}   placeholder="KEY"   class="input-key"   required />
            <input bind:value={newValue} placeholder="value" class="input-value" type="password" required />
            <button type="submit" class="btn-add" disabled={adding}>
              {adding ? '…' : 'Add'}
            </button>
          </form>
        {/if}
      </div>
    {:else}
      <div class="creds-empty">
        <p>Select a plugin to manage credentials.</p>
      </div>
    {/if}
  </div>
</div>

<style>
  .page { padding: var(--space-8); max-width: 1100px; margin: 0 auto; }
  h1 { font-size: 1.75rem; font-weight: 700; margin-bottom: var(--space-6); }

  .tabs { display: flex; gap: var(--space-2); margin-bottom: var(--space-6); border-bottom: 1px solid var(--color-border); }
  .tab {
    padding: var(--space-2) var(--space-4);
    background: none; border: none; border-bottom: 2px solid transparent;
    color: var(--color-text-dim); cursor: pointer; font-size: 0.9rem; font-weight: 500;
    margin-bottom: -1px;
  }
  .tab.active { color: var(--color-text); border-bottom-color: var(--color-accent); }

  .layout { display: grid; grid-template-columns: 280px 1fr; gap: var(--space-6); }

  .plugin-list { display: flex; flex-direction: column; gap: var(--space-2); }
  .plugin-card {
    display: flex; flex-direction: column; gap: 2px;
    padding: var(--space-3) var(--space-4); background: var(--color-surface);
    border: 1px solid var(--color-border); border-radius: 6px;
    cursor: pointer; text-align: left; width: 100%;
  }
  .plugin-card:hover { border-color: var(--color-accent); }
  .plugin-card.selected { border-color: var(--color-accent); background: var(--color-surface-2); }
  .plugin-id { font-weight: 600; font-size: 0.9rem; }
  .plugin-endpoint { font-size: 0.75rem; color: var(--color-text-dim); font-family: monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .badge { font-size: 0.7rem; padding: 1px 6px; border-radius: 10px; width: fit-content; margin-top: 2px; }
  .badge.enabled { background: #16a34a22; color: #16a34a; }
  .badge.disabled { background: #dc262622; color: #dc2626; }

  .creds-panel { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: 8px; padding: var(--space-6); }
  .creds-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: var(--space-4); }
  h2 { font-size: 1.1rem; font-weight: 600; }

  .btn-sync {
    padding: var(--space-1) var(--space-3); font-size: 0.8rem;
    background: var(--color-accent); color: white; border: none; border-radius: 4px; cursor: pointer;
  }
  .btn-sync:hover { opacity: 0.85; }

  .creds-table { width: 100%; border-collapse: collapse; font-size: 0.875rem; margin-bottom: var(--space-4); }
  .creds-table th { text-align: left; padding: var(--space-2) var(--space-3); color: var(--color-text-dim); font-weight: 500; border-bottom: 1px solid var(--color-border); }
  .creds-table td { padding: var(--space-2) var(--space-3); border-bottom: 1px solid var(--color-border-dim, var(--color-border)); }
  .key { font-family: monospace; font-weight: 500; }
  .updated { font-size: 0.75rem; color: var(--color-text-dim); }
  .empty-row { text-align: center; color: var(--color-text-dim); padding: var(--space-6) !important; }

  .sync-badge { font-size: 0.72rem; padding: 2px 6px; border-radius: 10px; }
  .sync-badge.synced { background: #16a34a22; color: #16a34a; }
  .sync-badge:not(.synced) { background: #f59e0b22; color: #d97706; }

  .btn-remove { background: none; border: none; color: var(--color-text-dim); cursor: pointer; font-size: 1.1rem; padding: 0 4px; }
  .btn-remove:hover { color: #dc2626; }

  .add-cred { display: flex; gap: var(--space-2); margin-top: var(--space-2); }
  .input-key, .input-value {
    padding: var(--space-2) var(--space-3); border: 1px solid var(--color-border);
    border-radius: 4px; background: var(--color-bg); color: var(--color-text);
    font-family: monospace; font-size: 0.85rem;
  }
  .input-key { width: 160px; }
  .input-value { flex: 1; }
  .btn-add {
    padding: var(--space-2) var(--space-4); background: var(--color-accent); color: white;
    border: none; border-radius: 4px; cursor: pointer; font-size: 0.85rem;
  }
  .btn-add:disabled { opacity: 0.5; cursor: default; }

  .creds-empty { display: flex; align-items: center; justify-content: center; color: var(--color-text-dim); }
  .loading { color: var(--color-text-dim); }
  .empty { color: var(--color-text-dim); font-size: 0.875rem; }
</style>
