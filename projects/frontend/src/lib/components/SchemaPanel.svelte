<script lang="ts">
  import { page } from '$app/stores';
  import { onMount } from 'svelte';
  import { listSchemaDatabases } from '../api/client';
  import { getLinkedSchemas } from '../stores/sidebar.svelte';

  interface DbInfo { name: string; [k: string]: any }

  let allDbs = $state<DbInfo[]>([]);
  let open   = $state(false);

  let linkedSchemas = $derived(getLinkedSchemas());
  let dbs = $derived(allDbs.filter(db => !linkedSchemas.includes(db.name)));

  onMount(async () => {
    open = localStorage.getItem('sidebar-schema-open') === '1';
    try { allDbs = (await listSchemaDatabases() as any[]) ?? []; } catch {}
  });

  function toggle() {
    open = !open;
    localStorage.setItem('sidebar-schema-open', open ? '1' : '0');
  }

  let onSchema = $derived($page.url.pathname.startsWith('/schema'));
  let activeDb = $derived(onSchema ? ($page.url.searchParams.get('db') ?? dbs[0]?.name ?? '') : '');
</script>

{#if allDbs.length === 0 || dbs.length > 0}
<div class="schema-section">
  <button class="section-header" onclick={toggle}>
    <span class="arrow">{open ? '▾' : '▸'}</span>
    Schema
  </button>
  {#if open && dbs.length > 0}
    <div class="schema-body">
      {#each dbs as db (db.name)}
        <a
          href="/schema?db={encodeURIComponent(db.name)}"
          class="db-link"
          class:active={activeDb === db.name}
        >
          {db.name}
        </a>
      {/each}
    </div>
  {/if}
</div>
{/if}

<style>
  .schema-section { border-top: 1px solid var(--color-border); }
  .section-header {
    display: flex; align-items: center; gap: var(--space-1);
    width: 100%; background: none; border: none; cursor: pointer;
    padding: var(--space-1) var(--space-3);
    font-size: var(--text-xs); font-weight: var(--weight-semibold);
    color: var(--color-text-dim); text-transform: uppercase; letter-spacing: 0.05em;
    transition: color var(--transition-fast);
  }
  .section-header:hover { color: var(--color-text-muted); }
  .arrow { font-size: 0.6rem; opacity: 0.8; }
  .schema-body { padding-bottom: var(--space-1); }
  .db-link {
    display: block; padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-sm); margin: 0 var(--space-1);
    font-size: var(--text-xs); font-family: var(--font-mono);
    color: var(--color-text-dim); text-decoration: none;
    transition: color var(--transition-fast), background var(--transition-fast);
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  }
  .db-link:hover { color: var(--color-text); background: var(--color-surface-2); }
  .db-link.active { color: var(--color-accent); background: rgba(124,106,247,0.1); border-left: 2px solid var(--color-accent); }
</style>
