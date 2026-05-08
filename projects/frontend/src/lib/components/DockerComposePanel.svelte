<script lang="ts">
  import { onMount } from 'svelte';
  import StatusDot from './StatusDot.svelte';
  import type { DockerService } from '../types/sidebar';

  interface Props {
    projectPath: string;
    composeName?: string;
    busy?: boolean;
    onoutput?: (output: string, label: string) => void;
    onservicesloaded?: (services: DockerService[]) => void;
  }

  let { projectPath, composeName, busy = false, onoutput, onservicesloaded }: Props = $props();

  let services    = $state<DockerService[]>([]);
  let composeFile = $state<string | null>(null);
  let localBusy   = $state(false);
  let openDd      = $state<string | null>(null);

  const anyBusy   = $derived(busy || localBusy);
  const anyUp     = $derived(services.some(s => s.running));
  const fileLabel = $derived(composeName ?? (composeFile ? composeFile.split('/').pop()! : ''));

  async function loadServices() {
    try {
      const res = await fetch(`/api/docker/services?path=${encodeURIComponent(projectPath)}`);
      const d   = await res.json();
      services    = (d.services ?? []).sort((a: DockerService, b: DockerService) => a.name.localeCompare(b.name));
      composeFile = d.composeFile ?? null;
      onservicesloaded?.(services);
    } catch {}
  }

  async function act(action: string, service?: string) {
    localBusy = true; openDd = null;
    try {
      const res = await fetch('/api/docker/action', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ projectPath, action, service }),
      });
      const d = await res.json();
      onoutput?.(d.output ?? '', service ? `${service} ${action}` : action);
      if (action !== 'logs') setTimeout(loadServices, 1200);
    } catch (e: any) {
      onoutput?.(`Error: ${e.message ?? 'unknown'}`, action);
    } finally { localBusy = false; }
  }

  function toggleDd(key: string) { openDd = openDd === key ? null : key; }

  onMount(loadServices);
</script>

{#if openDd}
  <div class="dd-overlay" role="presentation" onclick={() => openDd = null} onkeydown={() => openDd = null}></div>
{/if}

<div class="compose-section">
  <!-- Compose row — same grid layout as container rows -->
  <div class="ctr-row">
    <StatusDot ok={anyUp} />
    <span class="ctr-name">{fileLabel}</span>
    <span></span><!-- health placeholder -->
    <span></span><!-- ports placeholder -->
    <div class="dd-wrap">
      <button class="cbtn cbtn-icon" title="Compose actions" onclick={() => toggleDd('compose')}>⋯</button>
      {#if openDd === 'compose'}
        <div class="dd-menu dd-menu-left">
          <button class="dd-item dd-item-primary" disabled={anyBusy} onclick={() => act('up')}>Up</button>
          <button class="dd-item" disabled={anyBusy} onclick={() => act('down')}>Down</button>
          <button class="dd-item" disabled={anyBusy} onclick={() => act('restart')}>Restart</button>
          <button class="dd-item" disabled={anyBusy} onclick={() => act('pull')}>Pull</button>
          <div class="dd-sep"></div>
          <button class="dd-item" onclick={loadServices}>Refresh</button>
        </div>
      {/if}
    </div>
  </div>

  <!-- Container rows -->
  {#each services as svc (svc.name)}
    <div class="ctr-row">
      <StatusDot ok={svc.running} />
      <span class="ctr-name">{svc.name}</span>
      {#if svc.health}<span class="ctr-badge">{svc.health}</span>{:else}<span></span>{/if}
      <span class="ctr-ports">{svc.ports.slice(0, 2).join(' · ')}</span>
      <div class="dd-wrap">
        <button class="cbtn cbtn-icon" title="Container actions" onclick={() => toggleDd(svc.name)}>⋯</button>
        {#if openDd === svc.name}
          <div class="dd-menu dd-menu-left">
            <button class="dd-item dd-item-primary" disabled={anyBusy} onclick={() => act('up', svc.name)}>Up</button>
            <button class="dd-item" disabled={anyBusy} onclick={() => act('stop', svc.name)}>Stop</button>
            <button class="dd-item" disabled={anyBusy} onclick={() => act('restart', svc.name)}>Restart</button>
            <div class="dd-sep"></div>
            <button class="dd-item" disabled={anyBusy} onclick={() => act('logs', svc.name)}>Logs</button>
          </div>
        {/if}
      </div>
    </div>
  {/each}
</div>

<style>
  .dd-overlay { position: fixed; inset: 0; z-index: 10; }

  .dd-wrap { position: relative; }
  .dd-menu {
    position: absolute; right: 0; top: calc(100% + 2px); z-index: 11;
    background: var(--color-surface); border: 1px solid var(--color-border);
    border-radius: var(--radius-md); box-shadow: var(--shadow-md);
    min-width: 110px; padding: 4px;
    display: flex; flex-direction: column; gap: 1px;
  }
  .dd-menu-left { right: auto; left: 0; }
  .dd-item {
    background: none; border: none; cursor: pointer; text-align: left;
    font-size: var(--text-xs); color: var(--color-text-muted); font-family: var(--font-sans);
    padding: 5px 10px; border-radius: var(--radius-sm);
    transition: background var(--transition-fast), color var(--transition-fast);
    white-space: nowrap;
  }
  .dd-item:hover:not(:disabled) { background: var(--color-surface-2); color: var(--color-text); }
  .dd-item:disabled { opacity: 0.38; cursor: not-allowed; }
  .dd-item-primary { color: var(--color-accent); }
  .dd-item-primary:hover:not(:disabled) { background: rgba(124,106,247,0.1); }
  .dd-sep { height: 1px; background: var(--color-border); margin: 3px 0; }

  .compose-section { display: flex; flex-direction: column; }

  /* unified grid for all rows */
  .ctr-row {
    display: grid;
    grid-template-columns: 14px 1fr auto auto auto;
    align-items: center; gap: var(--space-2);
    padding: 4px 0;
    border-top: 1px solid var(--color-border);
  }
  .ctr-row:first-child { border-top: none; }

  .ctr-name  { font-size: var(--text-xs); font-family: var(--font-mono); font-weight: var(--weight-medium); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .ctr-badge { font-size: 9px; color: var(--color-text-dim); background: var(--color-surface-2); border-radius: var(--radius-sm); padding: 1px 5px; white-space: nowrap; }
  .ctr-ports { font-size: var(--text-xs); color: var(--color-text-dim); font-family: var(--font-mono); white-space: nowrap; }
</style>
