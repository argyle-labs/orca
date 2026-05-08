<script lang="ts">
  import type { TreeNode } from '$lib/api/types';

  let { data } = $props();
  const tree: Record<string, TreeNode[]> = $derived(data.tree ?? {});

  function countFiles(nodes: TreeNode[]): number {
    let n = 0;
    for (const node of nodes) {
      if (node.type === 'file') n++;
      else if (node.children) n += countFiles(node.children);
    }
    return n;
  }

  function countDirs(nodes: TreeNode[]): number {
    let n = 0;
    for (const node of nodes) {
      if (node.type === 'dir') { n++; if (node.children) n += countDirs(node.children); }
    }
    return n;
  }

  const ROOT_LABELS: Record<string, string> = {
    orca:    'Orca',
    rebuy:    'Rebuy',
    dotfiles: 'Dotfiles',
  };

  const ROOT_DESCS: Record<string, string> = {
    orca:    'Orca server, CLI, and frontend source code',
    rebuy:    'Rebuy platform repos and documentation',
    dotfiles: 'Personal config, notes, and vault',
  };
</script>

<svelte:head><title>Resources — orca</title></svelte:head>

<div class="page">
  <h1>Resources</h1>
  <p class="subtitle">Document roots available for browsing. Expand them in the sidebar to navigate files.</p>

  <div class="roots">
    {#each Object.entries(tree) as [root, nodes] (root)}
      {@const files = countFiles(nodes)}
      {@const dirs = countDirs(nodes)}
      <div class="root-card">
        <div class="root-header">
          <span class="root-name">{ROOT_LABELS[root] ?? root}</span>
          <span class="root-stats">{files} files · {dirs} dirs</span>
        </div>
        <p class="root-desc">{ROOT_DESCS[root] ?? ''}</p>
        {#if nodes.length > 0}
          <ul class="top-entries">
            {#each nodes.slice(0, 6) as node (node.path)}
              <li>
                {#if node.type === 'file'}
                  <a href="/{root}/{node.path.replace(/\.mdx?$/, '')}">
                    <span class="entry-icon">◻</span>{node.name}
                  </a>
                {:else}
                  <span class="entry-dir">
                    <span class="entry-icon">▸</span>{node.name}
                  </span>
                {/if}
              </li>
            {/each}
            {#if nodes.length > 6}
              <li class="more">+{nodes.length - 6} more — expand in sidebar</li>
            {/if}
          </ul>
        {/if}
      </div>
    {:else}
      <p style="color:var(--color-text-dim)">No doc roots available.</p>
    {/each}
  </div>
</div>

<style>
  .page { padding: var(--space-6) var(--space-8); max-width: 900px; }
  h1 { font-size: var(--text-xl); font-weight: var(--weight-bold); margin-bottom: var(--space-2); }
  .subtitle { color: var(--color-text-dim); font-size: var(--text-sm); margin-bottom: var(--space-6); }

  .roots { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: var(--space-4); }

  .root-card {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    padding: var(--space-4);
  }
  .root-header {
    display: flex; align-items: baseline; justify-content: space-between;
    margin-bottom: var(--space-1);
  }
  .root-name { font-weight: var(--weight-semibold); font-size: var(--text-base); }
  .root-stats { font-size: var(--text-xs); color: var(--color-text-faint); font-family: var(--font-mono); }
  .root-desc { font-size: var(--text-xs); color: var(--color-text-dim); margin-bottom: var(--space-3); }

  .top-entries { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 1px; }
  .top-entries li a, .entry-dir {
    display: flex; align-items: center; gap: var(--space-1);
    font-size: var(--text-xs); color: var(--color-text-dim);
    padding: 2px 0; transition: color var(--transition-fast);
  }
  .top-entries li a:hover { color: var(--color-text); text-decoration: none; }
  .entry-icon { font-size: 9px; width: 12px; flex-shrink: 0; opacity: 0.6; }
  .entry-dir { color: var(--color-text-faint); }
  .more { font-size: var(--text-xs); color: var(--color-text-faint); padding: var(--space-1) 0; font-style: italic; }
</style>
