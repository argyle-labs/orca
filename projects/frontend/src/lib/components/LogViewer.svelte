<script lang="ts">
  import { onMount } from 'svelte';
  import StatusDot from './StatusDot.svelte';
  import type { DockerService } from '../types/sidebar';

  interface Props {
    projectPath: string;
    services: DockerService[];
    actionOutput?: string;   // unified output from MCP or compose actions
    actionLabel?: string;    // label shown in the output tab
    busy?: boolean;
  }

  let { projectPath, services, actionOutput = '', actionLabel = '', busy = false }: Props = $props();

  // When a new action output arrives, switch to the output tab.
  $effect(() => { if (actionOutput) logTab = '__output__'; });

  let logTab = $state('all');
  let logs   = $state('');
  let search = $state('');
  let tail   = $state('200');

  async function loadLogs() {
    if (logTab === '__output__') return; // output tab is driven by prop
    try {
      const svc = logTab === 'all' ? undefined : logTab;
      const res = await fetch('/api/docker/action', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ projectPath, action: 'logs', service: svc, tail: Number(tail) }),
      });
      const d = await res.json();
      logs = d.output ?? '';
    } catch {}
  }

  onMount(loadLogs);
  $effect(() => { void logTab; void tail; if (logTab !== '__output__') loadLogs(); });

  const displayText = $derived(logTab === '__output__' ? actionOutput : logs);
  const filtered = $derived(
    search.trim()
      ? displayText.split('\n').filter(l => l.toLowerCase().includes(search.toLowerCase())).join('\n')
      : displayText
  );
</script>

<div class="log-wrap">
  <div class="log-tabs">
    {#if actionOutput}
      <button class="cbtn cbtn-icon log-tab {logTab === '__output__' ? 'active' : ''}"
              onclick={() => logTab = '__output__'}>
        ⌘ {actionLabel || 'Output'}
      </button>
    {/if}
    <button class="cbtn cbtn-icon log-tab {logTab === 'all' ? 'active' : ''}"
            onclick={() => logTab = 'all'}>All</button>
    {#each services as svc (svc.name)}
      <button class="cbtn cbtn-icon log-tab {logTab === svc.name ? 'active' : ''}"
              onclick={() => logTab = svc.name}>
        <StatusDot ok={svc.running} />{svc.name}
      </button>
    {/each}
  </div>
  <div class="log-controls">
    <input class="log-search" type="text" placeholder="Filter…" bind:value={search} />
    {#if logTab !== '__output__'}
      <select class="log-tail" bind:value={tail}>
        {#each ['50','100','200','500','1000'] as n (n)}<option value={n}>{n}</option>{/each}
      </select>
      <button class="cbtn cbtn-icon" onclick={loadLogs} disabled={busy}>↺</button>
    {/if}
  </div>
  <div class="scroll-area">
    <pre class="log-output">{filtered || (logTab === '__output__' ? 'No output' : 'No logs')}</pre>
  </div>
</div>

<style>
  .log-wrap { display: flex; flex-direction: column; flex: 1; min-height: 0; }
  .log-tabs { display: flex; gap: 2px; flex-wrap: wrap; margin-bottom: var(--space-1); flex-shrink: 0; }
  .log-tab { font-size: var(--text-xs); display: flex; align-items: center; gap: 4px; }
  .log-tab.active { color: var(--color-accent); border-color: rgba(124,106,247,0.35); background: rgba(124,106,247,0.08); }
  .log-controls { display: flex; gap: var(--space-2); margin-bottom: var(--space-1); align-items: center; flex-shrink: 0; }
  .log-search {
    flex: 1; background: var(--color-bg); border: 1px solid var(--color-border);
    border-radius: var(--radius-sm); color: var(--color-text); font-size: var(--text-xs);
    padding: 2px 8px; outline: none;
  }
  .log-search:focus { border-color: var(--color-accent); }
  .log-tail {
    background: var(--color-bg); border: 1px solid var(--color-border);
    border-radius: var(--radius-sm); color: var(--color-text); font-size: var(--text-xs);
    padding: 2px 6px; width: 66px; outline: none;
  }
  .scroll-area { flex: 1; min-height: 180px; overflow-y: auto; }
  .log-output {
    font-family: var(--font-mono); font-size: var(--text-xs); white-space: pre-wrap;
    word-break: break-all; background: var(--color-bg);
    border: 1px solid var(--color-border); border-radius: var(--radius-md);
    padding: var(--space-3); margin: 0; height: 100%;
  }
</style>
