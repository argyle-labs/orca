<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import ModalShell from './ModalShell.svelte';

  let { open, onclose }: { open: boolean; onclose: () => void } = $props();

  interface DocResult   { kind: 'doc';   root: string; path: string; matches: string[] }
  interface TableResult { kind: 'table'; table: string; domain: string; group: string; color: string }
  interface PageResult  { kind: 'page';  href: string; label: string }

  const ALL_PAGES: PageResult[] = [
    { kind: 'page', href: '/plugins',    label: 'Plugins' },
    { kind: 'page', href: '/settings',   label: 'Settings' },
    { kind: 'page', href: '/system',     label: 'System' },
    { kind: 'page', href: '/resources',  label: 'Resources' },
    { kind: 'page', href: '/schema',     label: 'Schema' },
    { kind: 'page', href: '/api-docs',   label: 'API Docs' },
    { kind: 'page', href: '/graphql',    label: 'GraphQL' },
    { kind: 'page', href: '/mcps',       label: 'MCPs' },
    { kind: 'page', href: '/ctx7',       label: 'Library Docs' },
    { kind: 'page', href: '/jira',       label: 'Jira' },
    { kind: 'page', href: '/confluence', label: 'Confluence' },
    { kind: 'page', href: '/bitbucket',  label: 'Bitbucket' },
  ];

  type FilterPill = 'all' | 'docs' | 'pages' | 'tables';

  let query   = $state('');
  let filter  = $state<FilterPill>('all');
  let docs    = $state<DocResult[]>([]);
  let tables  = $state<TableResult[]>([]);
  let domains = $state<Domain[]>([]);
  let cursor  = $state(0);
  let inputEl: HTMLInputElement | null = $state(null);
  let docTimer: ReturnType<typeof setTimeout> | null = null;

  interface Domain { key: string; label: string; group?: string; color: string; tables: string[] }

  async function loadDomains() {
    if (domains.length > 0) return;
    try {
      const r = await fetch('/api/schema/domains');
      if (r.ok) domains = await r.json();
    } catch {}
  }

  onMount(loadDomains);

  $effect(() => {
    if (open) {
      query = ''; docs = []; tables = []; cursor = 0; filter = 'all';
      setTimeout(() => inputEl?.focus(), 10);
    }
  });

  /** Pages filtered by query (or all if no query). */
  let filteredPages = $derived<PageResult[]>(
    query.trim()
      ? ALL_PAGES.filter(p => p.label.toLowerCase().includes(query.toLowerCase()))
      : ALL_PAGES
  );

  $effect(() => {
    if (docTimer) clearTimeout(docTimer);

    if (!query.trim()) {
      docs = []; tables = []; cursor = 0;
      return;
    }

    const q = query.toLowerCase();
    const matched: TableResult[] = [];
    for (const d of domains) {
      for (const t of d.tables) {
        if (t.toLowerCase().includes(q)) {
          matched.push({ kind: 'table', table: t, domain: d.label, group: d.group ?? d.label, color: d.color });
          if (matched.length >= 12) break;
        }
      }
      if (matched.length >= 12) break;
    }
    tables = matched;

    docTimer = setTimeout(async () => {
      try {
        const r = await fetch(`/api/search?q=${encodeURIComponent(query)}&root=all`);
        const data = await r.json();
        docs = data.map((r: any) => ({ ...r, kind: 'doc' as const }));
        cursor = 0;
      } catch {}
    }, 200);
  });

  type AnyResult = DocResult | TableResult | PageResult | { kind: 'ctx7' };

  /** Visible docs (capped, filter-aware). */
  let visibleDocs = $derived<DocResult[]>(
    (filter === 'all' || filter === 'docs') ? docs.slice(0, 8) : []
  );
  /** Visible tables (capped, filter-aware). */
  let visibleTables = $derived<TableResult[]>(
    (filter === 'all' || filter === 'tables') ? tables.slice(0, 6) : []
  );
  /** Visible pages (filter-aware). */
  let visiblePages = $derived<PageResult[]>(
    (filter === 'all' || filter === 'pages') ? filteredPages : []
  );
  /** Show ctx7 option when query is long enough and filter allows docs. */
  let showCtx7 = $derived(
    query.trim().length >= 2 && (filter === 'all' || filter === 'docs')
  );

  let allResults = $derived<AnyResult[]>([
    ...visiblePages,
    ...visibleDocs,
    ...visibleTables,
    ...(showCtx7 ? [{ kind: 'ctx7' as const }] : []),
  ]);

  function activate(item: AnyResult) {
    if (item.kind === 'page')  { goto((item as PageResult).href); onclose(); }
    if (item.kind === 'doc')   { goto(`/${(item as DocResult).root}/${(item as DocResult).path.replace(/\.mdx?$/, '')}`); onclose(); }
    if (item.kind === 'table') { goto('/schema'); onclose(); }
    if (item.kind === 'ctx7')  { goto(`/ctx7?q=${encodeURIComponent(query)}`); onclose(); }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'ArrowDown') { e.preventDefault(); cursor = Math.min(cursor + 1, allResults.length - 1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); cursor = Math.max(cursor - 1, 0); }
    else if (e.key === 'Enter') { const item = allResults[cursor]; if (item) activate(item); }
  }

  /** Compute the flat cursor index for a given item within allResults. */
  function idxOf(item: AnyResult): number {
    return allResults.indexOf(item);
  }
</script>

<ModalShell {open} {onclose} align="top">
  <div class="modal">
    <div class="header">
      <svg class="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8">
        <circle cx="6.5" cy="6.5" r="4.5"/><line x1="10.5" y1="10.5" x2="14" y2="14"/>
      </svg>
      <!-- svelte-ignore a11y_autofocus -->
      <input bind:this={inputEl} class="input" placeholder="Search pages, docs, tables…"
             bind:value={query} onkeydown={handleKeydown} />
      {#if query}
        <button class="clear" onclick={() => { query = ''; inputEl?.focus(); }}>✕</button>
      {/if}
      <kbd class="esc">esc</kbd>
    </div>

    <!-- Filter pills -->
    <div class="filter-row">
      {#each (['all', 'pages', 'docs', 'tables'] as FilterPill[]) as pill (pill)}
        <button
          class="pill {filter === pill ? 'active' : ''}"
          onclick={() => { filter = pill; cursor = 0; }}
        >
          {pill === 'all' ? 'All' : pill === 'pages' ? 'Pages' : pill === 'docs' ? 'Docs' : 'Tables'}
        </button>
      {/each}
    </div>

    <div class="results">
      <!-- Pages / Quick Nav -->
      {#if visiblePages.length > 0}
        <div class="group-label">{query.trim() ? 'Pages' : 'Quick Nav'}</div>
        {#each visiblePages as p (p.href)}
          {@const i = idxOf(p)}
          <button class="result page-result {cursor === i ? 'active' : ''}"
                  onclick={() => activate(p)} onmouseenter={() => cursor = i}>
            <span class="result-path">
              <svg class="page-icon" width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8">
                <rect x="2" y="1" width="12" height="14" rx="1.5"/>
                <line x1="5" y1="5" x2="11" y2="5"/>
                <line x1="5" y1="8" x2="11" y2="8"/>
                <line x1="5" y1="11" x2="8" y2="11"/>
              </svg>
              {p.label}
            </span>
            <span class="result-match">{p.href}</span>
          </button>
        {/each}
      {/if}

      <!-- Docs -->
      {#if visibleDocs.length > 0}
        <div class="group-label">Docs</div>
        {#each visibleDocs as r (r.root + '/' + r.path)}
          {@const i = idxOf(r)}
          <button class="result {cursor === i ? 'active' : ''}"
                  onclick={() => activate(r)} onmouseenter={() => cursor = i}>
            <span class="result-path">{r.root}/{r.path.replace(/\.mdx?$/, '')}</span>
            {#if r.matches[0]}<span class="result-match">{r.matches[0]}</span>{/if}
          </button>
        {/each}
      {/if}

      <!-- DB Tables -->
      {#if visibleTables.length > 0}
        <div class="group-label">DB Tables</div>
        {#each visibleTables as r (r.table)}
          {@const i = idxOf(r)}
          <button class="result {cursor === i ? 'active' : ''}"
                  onclick={() => activate(r)} onmouseenter={() => cursor = i}>
            <span class="result-path">
              <span class="dot" style="background:{r.color}"></span>{r.table}
            </span>
            <span class="result-match">{r.group} → {r.domain}</span>
          </button>
        {/each}
      {/if}

      <!-- Library Docs (ctx7) -->
      {#if showCtx7}
        {@const i = idxOf({ kind: 'ctx7' })}
        <div class="group-label">Library Docs</div>
        <button class="result ctx7 {cursor === i ? 'active' : ''}"
                onclick={() => activate({ kind: 'ctx7' })} onmouseenter={() => cursor = i}>
          <span class="result-path">Search "{query}" in library docs via Context7 →</span>
          <span class="result-match">Browse React, Next.js, Prisma, and more</span>
        </button>
      {/if}

      {#if allResults.length === 0 && query.trim().length >= 2}
        <div class="empty">No matches found</div>
      {/if}
    </div>
  </div>
</ModalShell>

<style>
  .modal { width:min(600px,92vw); background:var(--color-surface); border:1px solid var(--color-border); border-radius:var(--radius-lg); box-shadow:var(--shadow-lg); overflow:hidden; }
  .header { display:flex; align-items:center; gap:var(--space-2); padding:var(--space-3) var(--space-4); border-bottom:1px solid var(--color-border); }
  .icon { color:var(--color-text-dim); flex-shrink:0; }
  .input { flex:1; background:none; border:none; outline:none; font-size:var(--text-base); color:var(--color-text); }
  .input::placeholder { color:var(--color-text-faint); }
  .clear { background:none; border:none; cursor:pointer; color:var(--color-text-dim); font-size:var(--text-xs); padding:2px 4px; border-radius:var(--radius-sm); }
  .clear:hover { color:var(--color-text); }
  .esc { font-family:var(--font-mono); font-size:var(--text-xs); background:var(--color-surface-2); border:1px solid var(--color-border); border-radius:var(--radius-sm); padding:1px 5px; color:var(--color-text-faint); }

  /* Filter pills */
  .filter-row { display:flex; gap:var(--space-1); padding:var(--space-2) var(--space-3); border-bottom:1px solid var(--color-border); }
  .pill { background:none; border:1px solid var(--color-border); border-radius:var(--radius-sm); padding:2px var(--space-2); font-size:var(--text-xs); color:var(--color-text-dim); cursor:pointer; transition:color var(--transition-fast), background var(--transition-fast), border-color var(--transition-fast); }
  .pill:hover { color:var(--color-text); background:var(--color-surface-2); }
  .pill.active { color:var(--color-accent); background:rgba(124,106,247,0.12); border-color:rgba(124,106,247,0.3); }

  .results { max-height:440px; overflow-y:auto; padding:var(--space-1); }
  .group-label { font-size:var(--text-xs); color:var(--color-text-dim); font-weight:var(--weight-semibold); text-transform:uppercase; letter-spacing:0.05em; padding:var(--space-2) var(--space-3) var(--space-1); }
  .result { display:flex; flex-direction:column; gap:2px; width:100%; background:none; border:none; text-align:left; padding:var(--space-2) var(--space-3); border-radius:var(--radius-md); cursor:pointer; transition:background var(--transition-fast); }
  .result:hover, .result.active { background:var(--color-surface-2); }
  .result-path { font-size:var(--text-sm); color:var(--color-text); display:flex; align-items:center; gap:var(--space-1); }
  .result-match { font-size:var(--text-xs); color:var(--color-text-dim); }
  .dot { width:8px; height:8px; border-radius:50%; display:inline-block; flex-shrink:0; }
  .ctx7 .result-path { color:var(--color-accent); }
  .page-icon { color:var(--color-text-dim); flex-shrink:0; }
  .page-result .result-path { color:var(--color-text); }
  .empty { padding:var(--space-4); text-align:center; color:var(--color-text-dim); font-size:var(--text-sm); }
</style>
