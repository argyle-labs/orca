<script lang="ts">
  import { onMount } from 'svelte';

  interface MeerkatPlugin {
    id: string;
    tier: string;
    enabled: boolean;
    mcpCommand: string;
    description?: string;
  }

  interface PluginStatus {
    plugin: MeerkatPlugin;
    ok: boolean | null;
  }

  let instances = $state<PluginStatus[]>([]);
  let loading = $state(true);

  function hostLabel(p: MeerkatPlugin): string {
    const id = p.id.replace('meerkat-', '').replace('meerkat', 'local');
    return id.charAt(0).toUpperCase() + id.slice(1);
  }

  function hostUrl(p: MeerkatPlugin): string {
    return p.mcpCommand ?? '';
  }

  async function checkHealth(p: MeerkatPlugin): Promise<boolean | null> {
    try {
      const res = await fetch(`/api/plugins/${p.id}/health`, { cache: 'no-store' });
      if (res.ok) {
        const d = await res.json();
        return d.status === 'ok';
      }
      return false;
    } catch {
      return false;
    }
  }

  async function load() {
    loading = true;
    try {
      const res = await fetch('/api/plugins');
      if (!res.ok) return;
      const all: MeerkatPlugin[] = await res.json();
      const meerkats = all.filter(p => p.tier === 'homelab' && p.mcpCommand);
      const statuses = await Promise.all(meerkats.map(async p => ({
        plugin: p,
        ok: await checkHealth(p),
      })));
      instances = statuses;
    } catch {
      instances = [];
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    load();
    const t = setInterval(load, 30_000);
    return () => clearInterval(t);
  });
</script>

<div class="homelab-panel">
  <div class="panel-header">
    <span class="panel-title">Homelab</span>
    <button class="refresh-btn" onclick={load} title="Refresh">↺</button>
  </div>

  {#if loading && instances.length === 0}
    <div class="row dim">checking...</div>
  {:else if instances.length === 0}
    <div class="row dim">no meerkat instances</div>
  {:else}
    {#each instances as { plugin, ok } (plugin.id)}
      <div class="row">
        <span class="dot {ok === true ? 'ok' : ok === false ? 'fail' : 'unknown'}">●</span>
        <span class="label">{hostLabel(plugin)}</span>
        {#if hostUrl(plugin)}
          <span class="host dim">{hostUrl(plugin).replace(/^https?:\/\//, '')}</span>
        {/if}
      </div>
    {/each}
  {/if}
</div>

<style>
  .homelab-panel {
    border-top: 1px solid var(--color-border);
    padding: var(--space-2) 0;
    flex-shrink: 0;
  }
  .panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-1) var(--space-3);
    margin-bottom: 2px;
  }
  .panel-title {
    font-size: var(--text-xs);
    font-weight: var(--weight-semibold);
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .refresh-btn {
    background: none;
    border: none;
    cursor: pointer;
    color: var(--color-text-dim);
    font-size: 12px;
    padding: 2px 4px;
    border-radius: var(--radius-sm);
  }
  .refresh-btn:hover { color: var(--color-text); background: var(--color-surface-2); }
  .row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 3px var(--space-3);
    font-size: var(--text-xs);
  }
  .dot { font-size: 8px; flex-shrink: 0; }
  .dot.ok    { color: #4ade80; }
  .dot.fail  { color: #f87171; }
  .dot.unknown { color: var(--color-text-dim); }
  .label { color: var(--color-text-muted); flex-shrink: 0; }
  .host { font-family: var(--font-mono); font-size: 10px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .dim { color: var(--color-text-dim); }
</style>
