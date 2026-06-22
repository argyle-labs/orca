<script lang="ts">
  import type { TopologyNode, TopologyEdge, NodeKind } from '$lib/client/types.gen';

  type Props = {
    nodes: TopologyNode[];
    edges: TopologyEdge[];
    onSelect?: (id: string) => void;
  };

  let { nodes, edges, onSelect }: Props = $props();

  // ── Data shaping ────────────────────────────────────────────────────────────
  //
  // The backend already gives us:
  //   - `parent_id` on each node (cluster → host membership)
  //   - parent_peer / mac_claim edges (host → guest)
  // We compose those into a single parent map so the layout is purely
  // hierarchical. Cluster compounds wrap their member hosts as a labelled
  // band; guests sit under their host inside the same band.

  type Tree = { node: TopologyNode; children: Tree[]; depth: number };

  function parentMap(ns: TopologyNode[], es: TopologyEdge[]): Map<string, string> {
    const m = new Map<string, string>();
    // Cluster membership wins as outer container.
    for (const n of ns) {
      if (n.parent_id) m.set(n.id, n.parent_id);
    }
    // parent_peer edges (host → guest) overlay on top, since the guest's
    // "parent" inside the band is its host, not the cluster directly.
    for (const e of es) {
      if (e.kind === 'parent_peer' || e.kind === 'mac_claim') {
        m.set(e.target, e.source);
      }
    }
    return m;
  }

  function buildForest(ns: TopologyNode[], es: TopologyEdge[]): Tree[] {
    const parents = parentMap(ns, es);
    const byId = new Map(ns.map((n) => [n.id, n]));
    const trees = new Map<string, Tree>();
    for (const n of ns) {
      trees.set(n.id, { node: n, children: [], depth: 0 });
    }
    const roots: Tree[] = [];
    for (const t of trees.values()) {
      const p = parents.get(t.node.id);
      if (p && trees.has(p)) {
        trees.get(p)!.children.push(t);
      } else {
        roots.push(t);
      }
    }
    // Stable visual order: clusters first, then hosts, then guests; within a
    // kind, alphabetic by label.
    const kindRank: Record<NodeKind, number> = {
      cluster: 0,
      host: 1,
      vm: 2,
      lxc: 2,
      container: 3,
      internet: 4,
    };
    function sortRec(list: Tree[], depth: number) {
      list.sort((a, b) => {
        const ka = kindRank[a.node.kind] - kindRank[b.node.kind];
        return ka !== 0 ? ka : a.node.label.localeCompare(b.node.label);
      });
      for (const t of list) {
        t.depth = depth;
        sortRec(t.children, depth + 1);
      }
    }
    sortRec(roots, 0);
    void byId; // map kept for future per-node lookups
    return roots;
  }

  let forest = $derived(buildForest(nodes, edges));

  const KIND_GLYPH: Record<NodeKind, string> = {
    host: '🖥',
    vm: '📦',
    lxc: '📦',
    container: '🐳',
    internet: '☁',
    cluster: '🌐',
  };

  function handleClick(n: TopologyNode) {
    if (n.kind === 'cluster') return;
    onSelect?.(n.id);
  }
</script>

<div class="topo-tree">
  {#each forest as root (root.node.id)}
    {@render branch(root)}
  {/each}
</div>

{#snippet branch(t: Tree)}
  {#if t.node.kind === 'cluster'}
    <div class="cluster">
      <div class="cluster-label">{t.node.label}</div>
      <div class="cluster-children">
        {#each t.children as c (c.node.id)}
          {@render branch(c)}
        {/each}
      </div>
    </div>
  {:else}
    <div class="branch">
      <button
        type="button"
        class="node node-{t.node.kind} status-{t.node.status}"
        onclick={() => handleClick(t.node)}
        title={t.node.id}
      >
        <span class="glyph">{KIND_GLYPH[t.node.kind]}</span>
        <span class="label">{t.node.label}</span>
      </button>
      {#if t.children.length}
        <div class="connector" aria-hidden="true"></div>
        <div class="children">
          {#each t.children as c (c.node.id)}
            {@render branch(c)}
          {/each}
        </div>
      {/if}
    </div>
  {/if}
{/snippet}

<style>
  .topo-tree {
    width: 100%;
    min-height: 400px;
    padding: var(--space-5);
    background: var(--color-surface-1, #0f172a);
    border-radius: var(--radius-md, 8px);
    border: 1px solid var(--color-border, #1e293b);
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    gap: var(--space-5);
    overflow-x: auto;
  }

  .cluster {
    display: flex;
    flex-direction: column;
    border: 1px dashed #f59e0b;
    border-radius: 10px;
    padding: 14px 18px 18px;
    background: rgba(245, 158, 11, 0.04);
  }
  .cluster-label {
    font-weight: 700;
    color: #fbbf24;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    font-size: 11px;
    margin-bottom: 10px;
  }
  .cluster-children {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    gap: var(--space-5);
  }

  .branch {
    display: flex;
    flex-direction: column;
    align-items: center;
    min-width: 120px;
  }

  .node {
    appearance: none;
    border: 2px solid #1e293b;
    border-radius: 10px;
    padding: 8px 14px;
    background: #3b82f6;
    color: #fff;
    font-weight: 600;
    font-size: 13px;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    white-space: nowrap;
    box-shadow: 0 2px 6px rgba(0, 0, 0, 0.3);
  }
  .node:hover { filter: brightness(1.1); }
  .node:focus-visible { outline: 2px solid #facc15; outline-offset: 2px; }
  .node-vm, .node-lxc { background: #8b5cf6; }
  .node-container    { background: #06b6d4; }
  .node-internet     { background: #94a3b8; color: #0f172a; }
  .status-down       { border-color: #ef4444; border-width: 3px; }

  .glyph { font-size: 14px; line-height: 1; }
  .label { font-family: var(--font-mono, ui-monospace, monospace); }

  .connector {
    width: 2px;
    height: 18px;
    background: #475569;
  }
  .children {
    display: flex;
    flex-wrap: wrap;
    justify-content: center;
    align-items: flex-start;
    gap: var(--space-4);
    position: relative;
    padding-top: 4px;
  }
  /* Horizontal rail joining sibling children under one parent. Only drawn
     when there's more than one child — single-child branches use the
     vertical connector alone. */
  .children::before {
    content: '';
    position: absolute;
    top: 0;
    left: 12px;
    right: 12px;
    height: 2px;
    background: #475569;
  }
  .branch > .children:has(> .branch:only-child)::before {
    display: none;
  }
</style>
