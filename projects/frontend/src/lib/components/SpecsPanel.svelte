<script lang="ts">
  import { page } from '$app/stores';
  import { onMount } from 'svelte';
  import { listSpecs, syncMcpSpecs } from '../api/client';
  import { getSection } from '../stores/mode.svelte';
  import { getLinkedSpecRepos } from '../stores/sidebar.svelte';

  interface Spec { repo: string; namespace?: string; hasGraphql?: boolean; files?: { full?: any }; sourceMcp?: string }

  let { mcpServer = '' }: { mcpServer?: string } = $props();

  let allSpecs = $state<Spec[]>([]);
  let open     = $state(false);
  let syncing  = $state(false);

  let section = $derived(getSection());
  let linkedRepos = $derived(getLinkedSpecRepos());

  // Filter by namespace, then hide specs already shown inside a linked project row.
  let specs = $derived(allSpecs.filter(s => {
    const inSection = section === 'orca'
      ? (s.namespace ?? 'orca') === 'orca'
      : (s.namespace ?? 'orca') === section;
    return inSection && !linkedRepos.includes(s.repo);
  }));

  onMount(async () => {
    open = localStorage.getItem('sidebar-specs-open') === '1';
    await reload();
  });

  async function reload() {
    try { allSpecs = (await listSpecs() as any[]) ?? []; } catch {}
  }

  async function syncMcp() {
    const server = mcpServer || (section === 'orca' ? '' : section);
    if (!server) return;
    syncing = true;
    try {
      await syncMcpSpecs({ server });
      await reload();
    } catch {}
    syncing = false;
  }

  function toggle() {
    open = !open;
    localStorage.setItem('sidebar-specs-open', open ? '1' : '0');
  }

  function hasRest(s: Spec) { return s.files?.full != null && s.files.full !== false; }
  function hasGql(s: Spec)  { return !!s.hasGraphql; }

  // Determine the MCP server name for the sync button
  let syncServer = $derived(mcpServer || (section !== 'orca' ? section : ''));
</script>

<div class="specs-section">
  <div class="section-header-row">
    <button class="section-header" onclick={toggle}>
      <span class="arrow">{open ? '▾' : '▸'}</span>
      API Docs
    </button>
    {#if syncServer}
      <button class="sync-btn" onclick={syncMcp} disabled={syncing} title="Sync from {syncServer} MCP">
        {syncing ? '…' : '↻'}
      </button>
    {/if}
  </div>

  {#if open}
    {#if specs.length > 0}
      <div class="specs-body">
        {#each specs as spec (spec.repo)}
          <a
            href="/api-docs/{spec.repo}"
            class="spec-link"
            class:active={$page.url.pathname.startsWith('/api-docs') && ($page.params.repo === spec.repo || $page.url.pathname === `/api-docs/${spec.repo}`)}
          >
            <span class="spec-name">{spec.repo}</span>
            <span class="badges">
              {#if hasRest(spec)}<span class="badge rest">REST</span>{/if}
              {#if hasGql(spec)}<span class="badge gql">GQL</span>{/if}
            </span>
          </a>
        {/each}
      </div>
    {:else if syncServer}
      <div class="empty-hint">No specs — click ↻ to sync from {syncServer}</div>
    {/if}
  {/if}
</div>

<style>
  .specs-section { border-top: 1px solid var(--color-border); }
  .section-header-row { display: flex; align-items: center; }
  .section-header {
    flex: 1; display: flex; align-items: center; gap: var(--space-1);
    background: none; border: none; cursor: pointer;
    padding: var(--space-1) var(--space-3);
    font-size: var(--text-xs); font-weight: var(--weight-semibold);
    color: var(--color-text-dim); text-transform: uppercase; letter-spacing: 0.05em;
    transition: color var(--transition-fast);
  }
  .section-header:hover { color: var(--color-text-muted); }
  .arrow { font-size: 0.6rem; opacity: 0.8; }
  .sync-btn {
    background: none; border: none; cursor: pointer;
    color: var(--color-text-dim); font-size: 13px; padding: 2px var(--space-2);
    transition: color var(--transition-fast);
  }
  .sync-btn:hover:not(:disabled) { color: var(--color-accent); }
  .sync-btn:disabled { opacity: 0.4; cursor: default; }
  .specs-body { padding-bottom: var(--space-1); }
  .empty-hint {
    font-size: var(--text-xs); color: var(--color-text-dim);
    padding: var(--space-1) var(--space-3) var(--space-2);
    font-style: italic;
  }
  .spec-link {
    display: flex; align-items: center; justify-content: space-between;
    gap: var(--space-2); padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-sm); margin: 0 var(--space-1);
    font-size: var(--text-xs); font-family: var(--font-mono);
    color: var(--color-text-dim); text-decoration: none;
    transition: color var(--transition-fast), background var(--transition-fast);
  }
  .spec-link:hover { color: var(--color-text); background: var(--color-surface-2); }
  .spec-link.active { color: var(--color-accent); background: rgba(124,106,247,0.1); border-left: 2px solid var(--color-accent); }
  .spec-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; }
  .badges { display: flex; gap: 3px; flex-shrink: 0; }
  .badge { font-size: 9px; padding: 1px 4px; border-radius: 3px; font-weight: 600; }
  .badge.rest { background: rgba(124,106,247,0.15); color: var(--color-accent); }
  .badge.gql  { background: rgba(231,70,148,0.15); color: #e746a0; }
</style>
