<script lang="ts">
  import { page } from '$app/stores';
  import { onMount } from 'svelte';
  import ServicesPanel from './ServicesPanel.svelte';
  import ProjectsPanel from './ProjectsPanel.svelte';
  import SpecsPanel from './SpecsPanel.svelte';
  import SchemaPanel from './SchemaPanel.svelte';
  import type { TreeNode } from '../api/types';
  import { getTree, listPlugins } from '../api/client';
  import { getSection, setSection, ORCA_CORE_NAV, ORCA_DOC_ROOTS } from '../stores/mode.svelte';
  import { getSidebarOpen, setSidebarOpen, setSidebarSections } from '../stores/sidebar.svelte';

  let { onhealthopen }: { onhealthopen: () => void } = $props();

  let treeData    = $state<Record<string, TreeNode[]>>({});
  let sectionOpen = $state<Record<string, boolean>>({});
  let dirOpen     = $state<Record<string, boolean>>({});

  type NavLink    = { href: string; label: string };
  type NavGroup   = { label: string; children: NavLink[] };
  type NavPanel   = { panel: string };
  type NavEntry   = NavLink | NavGroup | NavPanel;
  type PluginInfo = { id: string; mode: string; enabled: boolean; navLinks: NavEntry[] };
  let plugins = $state<PluginInfo[]>([]);
  let navGroupOpen = $state<Record<string, boolean>>({});

  function toggleNavGroup(label: string) {
    const next = !navGroupOpen[label];
    navGroupOpen[label] = next;
    localStorage.setItem(`sidebar-navgroup-${label}`, next ? '1' : '0');
  }

  let section = $derived(getSection());

  let sections = $derived.by(() => {
    const seen = new Set<string>(['orca']);
    for (const p of plugins) if (p.enabled && p.mode) seen.add(p.mode);
    return Array.from(seen);
  });

  // Keep the shared store in sync so TopNav can read sections
  $effect(() => { setSidebarSections(sections); });

  // Persist open state to localStorage whenever it changes
  $effect(() => { localStorage.setItem('sidebar-open', getSidebarOpen() ? '1' : '0'); });

  let allEntries = $derived.by<NavEntry[]>(() => {
    const pluginEntries = plugins
      .filter(p => p.enabled && p.mode === section)
      .flatMap(p => p.navLinks ?? []);
    if (section === 'orca') return [...ORCA_CORE_NAV, ...pluginEntries];
    return pluginEntries;
  });

  let navEntries = $derived(
    allEntries.filter(e => !('panel' in e && e.panel)) as (NavLink | NavGroup)[]
  );

  let sidebarPanels = $derived(
    (allEntries.filter(e => 'panel' in e && e.panel) as NavPanel[]).map(e => e.panel)
  );

  let docRoots = $derived.by(() => {
    if (section === 'orca') return ORCA_DOC_ROOTS as string[];
    const roots = plugins
      .filter(p => p.enabled && p.mode === section)
      .flatMap(p => (p.navLinks ?? []).filter((l: NavEntry) => 'href' in l && (l as NavLink).href?.startsWith('//vault/')).map((l: NavEntry) => (l as NavLink).href.replace('//vault/', '')));
    return (roots.length ? roots : [section]) as string[];
  });

  let visibleRoots = $derived(docRoots.filter((r: string) => (treeData[r] ?? []).length > 0));

  function formatRootLabel(key: string) {
    const labels: Record<string, string> = { brain: 'Brain', dotfiles: 'Dotfiles', rebuy: 'Rebuy' };
    return labels[key] ?? key.replace(/-/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
  }

  function toggleSection(rootName: string) {
    const next = !sectionOpen[rootName];
    sectionOpen[rootName] = next;
    localStorage.setItem(`sidebar-root-${rootName}`, next ? '1' : '0');
  }

  function toggleDir(lsKey: string) {
    const next = !dirOpen[lsKey];
    dirOpen[lsKey] = next;
    localStorage.setItem(lsKey, next ? '1' : '0');
  }

  function closeNav() { setSidebarOpen(false); }

  onMount(async () => {
    setSidebarOpen(localStorage.getItem('sidebar-open') !== '0');

    const [treeResult, pluginsResult] = await Promise.allSettled([
      getTree({}),
      listPlugins(),
    ]);

    if (treeResult.status === 'fulfilled') {
      treeData = (treeResult.value ?? {}) as Record<string, TreeNode[]>;
      const validRoots = new Set(Object.keys(treeData));
      const s: Record<string, boolean> = {};
      for (const root of validRoots) {
        s[root] = localStorage.getItem(`sidebar-root-${root}`) === '1';
      }
      sectionOpen = s;
    }

    if (pluginsResult.status === 'fulfilled') {
      plugins = (pluginsResult.value ?? []) as PluginInfo[];
      const groups: Record<string, boolean> = {};
      for (const p of plugins) {
        for (const e of (p.navLinks ?? [])) {
          if ('children' in e && e.label) {
            groups[e.label] = localStorage.getItem(`sidebar-navgroup-${e.label}`) === '1';
          }
        }
      }
      navGroupOpen = groups;
    }
  });
</script>

<nav class="sidebar" class:open={getSidebarOpen()}>
    <!-- ── Nav links / groups ──────────────────────────────────────────── -->
    {#if navEntries.length > 0}
      <div class="sidebar-nav">
        {#each navEntries as entry (('href' in entry ? entry.href : entry.label))}
          {#if 'children' in entry}
            <button class="nav-group-header" onclick={() => toggleNavGroup(entry.label)}>
              <span class="nav-group-arrow">{navGroupOpen[entry.label] ? '▾' : '▸'}</span>
              {entry.label}
            </button>
            {#if navGroupOpen[entry.label]}
              <div class="nav-group-body">
                {#each entry.children as child (child.href)}
                  <a href={child.href} onclick={closeNav}
                     class="nav-link child {$page.url.pathname === child.href || $page.url.pathname.startsWith(child.href + '/') ? 'active' : ''}">
                    {child.label}
                  </a>
                {/each}
              </div>
            {/if}
          {:else}
            <a href={entry.href} onclick={closeNav}
               class="nav-link {$page.url.pathname === entry.href || $page.url.pathname.startsWith(entry.href + '/') ? 'active' : ''}">
              {entry.label}
            </a>
          {/if}
        {/each}
      </div>
    {/if}

    <!-- ── Doc tree ────────────────────────────────────────────────────── -->
    {#if visibleRoots.length > 0}
      <div class="tree-section">
        {#each visibleRoots as rootName (rootName)}
          {@render RootSection(rootName, treeData[rootName])}
        {/each}
      </div>
    {/if}

    <!-- ── Plugin-declared panels ─────────────────────────────────────── -->
    {#if sidebarPanels.includes('specs')}
      <SpecsPanel />
    {/if}
    {#if sidebarPanels.includes('schema')}
      <SchemaPanel />
    {/if}
    {#if sidebarPanels.includes('services')}
      <ServicesPanel ondetailopen={onhealthopen} />
    {/if}
    {#if sidebarPanels.includes('projects')}
      <ProjectsPanel />
    {/if}
  </nav>

{#snippet RootSection(rootName: string, tree: TreeNode[])}
  <div class="root-section">
    <button class="root-header" onclick={() => toggleSection(rootName)}>
      <span class="arrow">{sectionOpen[rootName] ? '▾' : '▸'}</span>
      {formatRootLabel(rootName)}
    </button>
    {#if sectionOpen[rootName]}
      <ul class="tree-root">
        {#each tree as node (node.path)}
          {@render TreeNodeSnippet(node, rootName, 0)}
        {/each}
      </ul>
    {/if}
  </div>
{/snippet}

{#snippet TreeNodeSnippet(node: TreeNode, rootName: string, depth: number)}
  {#if node.type === 'file'}
    <li>
      <a href="/{rootName}/{node.path.replace(/\.mdx?$/, '').replace(/\\/g, '/')}" onclick={closeNav}
         class="tree-file {$page.url.pathname === `/${rootName}/${node.path.replace(/\.mdx?$/, '')}` ? 'active' : ''}"
         style="padding-left:{12 + depth * 14}px">
        {node.name}
      </a>
    </li>
  {:else}
    {@const lsKey = `sidebar-dir-${rootName}-${node.path}`}
    <li>
      <button class="tree-dir" style="padding-left:{12 + depth * 14}px"
              onclick={() => toggleDir(lsKey)}>
        <span class="dir-arrow">{dirOpen[lsKey] ? '▼' : '▶'}</span>{node.name}
      </button>
      {#if dirOpen[lsKey] && node.children}
        <ul>
          {#each node.children as child (child.path)}
            {@render TreeNodeSnippet(child, rootName, depth + 1)}
          {/each}
        </ul>
      {/if}
    </li>
  {/if}
{/snippet}

<style>
  .sidebar {
    position: fixed;
    top: var(--nav-height);
    left: 0;
    height: calc(100vh - var(--nav-height));
    width: var(--sidebar-width);
    background: var(--color-surface);
    border-right: 1px solid var(--color-border);
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    overflow-x: hidden;
    z-index: 40;
    transform: translateX(-100%);
    transition: transform 200ms ease, visibility 200ms;
    visibility: hidden;
  }

  .sidebar.open {
    transform: translateX(0);
    visibility: visible;
  }

  .sidebar-nav { padding: var(--space-2) var(--space-2); display:flex; flex-direction:column; gap:1px; }
  .nav-link { display:block; padding:var(--space-1) var(--space-2); border-radius:var(--radius-md); font-size:var(--text-sm); color:var(--color-text-dim); transition:color var(--transition-fast), background var(--transition-fast); }
  .nav-link:hover { color:var(--color-text); background:var(--color-surface-2); text-decoration:none; }
  .nav-link.active { color:var(--color-accent); background:rgba(124,106,247,0.1); }
  .nav-link.child { font-size:var(--text-xs); padding-left:var(--space-4); }
  .nav-group-header { display:flex; align-items:center; gap:var(--space-1); width:100%; background:none; border:none; cursor:pointer; padding:var(--space-1) var(--space-2); font-size:var(--text-xs); font-weight:var(--weight-semibold); color:var(--color-text-dim); text-transform:uppercase; letter-spacing:0.05em; border-radius:var(--radius-sm); transition:color var(--transition-fast); }
  .nav-group-header:hover { color:var(--color-text-muted); }
  .nav-group-arrow { font-size:0.6rem; opacity:0.7; }
  .nav-group-body { display:flex; flex-direction:column; gap:1px; }

  .tree-section { border-top: 1px solid var(--color-border); padding-top: var(--space-1); }
  .root-section { padding: var(--space-1) 0; }
  .root-header { display:flex; align-items:center; gap:var(--space-1); width:100%; background:none; border:none; cursor:pointer; padding:var(--space-1) var(--space-3); font-size:var(--text-xs); font-weight:var(--weight-semibold); color:var(--color-text-dim); text-transform:uppercase; letter-spacing:0.05em; transition:color var(--transition-fast); }
  .root-header:hover { color:var(--color-text-muted); }
  .arrow { font-size:0.6rem; opacity:0.8; }

  .tree-root, ul { list-style:none; padding:0; margin:0; }
  .tree-file { display:block; padding:2px var(--space-3); font-size:var(--text-xs); color:var(--color-text-dim); white-space:nowrap; overflow:hidden; text-overflow:ellipsis; transition:color var(--transition-fast); }
  .tree-file:hover { color:var(--color-text); text-decoration:none; }
  .tree-file.active { color:var(--color-accent); }
  .tree-dir { display:flex; align-items:center; gap:var(--space-1); width:100%; background:none; border:none; cursor:pointer; font-size:var(--text-xs); font-weight:var(--weight-medium); color:var(--color-text-dim); white-space:nowrap; overflow:hidden; text-overflow:ellipsis; transition:color var(--transition-fast); }
  .tree-dir:hover { color:var(--color-text-muted); }
  .dir-arrow { font-size:9px; opacity:0.7; width:10px; flex-shrink:0; }
</style>
