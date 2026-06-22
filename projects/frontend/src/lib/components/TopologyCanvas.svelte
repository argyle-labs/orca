<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import cytoscape from 'cytoscape';
  import type { Core, ElementDefinition } from 'cytoscape';
  import fcose, { type FcoseLayoutOptions } from 'cytoscape-fcose';

  function fcoseLayout(opts: { randomize: boolean; nodeSeparation: number }): FcoseLayoutOptions {
    return { name: 'fcose', animate: true, randomize: opts.randomize, nodeSeparation: opts.nodeSeparation };
  }
  import type { TopologyNode, TopologyEdge, NodeKind, EdgeKind } from '$lib/client/types.gen';

  // Register the fcose layout once. Cytoscape's `use()` is idempotent for
  // the same extension reference, so module-scope registration is safe.
  cytoscape.use(fcose);

  type Props = {
    nodes: TopologyNode[];
    edges: TopologyEdge[];
    onSelect?: (id: string) => void;
  };

  let { nodes, edges, onSelect }: Props = $props();

  let container: HTMLDivElement;
  let cy: Core | null = null;

  const KIND_GLYPH: Record<NodeKind, string> = {
    host: '🖥️',
    vm: '📦',
    lxc: '📦',
    container: '🐳',
    internet: '☁️',
    cluster: '🏢',
  };

  const KIND_COLOR: Record<NodeKind, string> = {
    host: '#3b82f6',
    vm: '#8b5cf6',
    lxc: '#22c55e',
    container: '#06b6d4',
    internet: '#94a3b8',
    cluster: '#f59e0b',
  };

  const EDGE_STYLE: Record<EdgeKind, 'solid' | 'dashed'> = {
    mac_claim: 'solid',
    parent_peer: 'solid',
    nfs_mount: 'dashed',
    network: 'dashed',
  };

  function buildElements(ns: TopologyNode[], es: TopologyEdge[]): ElementDefinition[] {
    const out: ElementDefinition[] = [];
    for (const n of ns) {
      out.push({
        group: 'nodes',
        data: {
          id: n.id,
          label: `${KIND_GLYPH[n.kind]} ${n.label}`,
          kind: n.kind,
          status: n.status,
          color: KIND_COLOR[n.kind],
          parent: n.parent_id ?? undefined,
        },
      });
    }
    for (const e of es) {
      out.push({
        group: 'edges',
        data: {
          id: e.id,
          source: e.source,
          target: e.target,
          kind: e.kind,
          style: EDGE_STYLE[e.kind],
          label: e.label ?? '',
        },
      });
    }
    return out;
  }

  onMount(() => {
    cy = cytoscape({
      container,
      elements: buildElements(nodes, edges),
      style: [
        {
          selector: 'node',
          style: {
            label: 'data(label)',
            'background-color': 'data(color)',
            'background-opacity': 0.85,
            color: '#fff',
            'font-size': 14,
            'text-valign': 'center',
            'text-halign': 'center',
            'text-outline-width': 2,
            'text-outline-color': '#0f172a',
            width: 64,
            height: 64,
            'border-width': 2,
            'border-color': '#1e293b',
          },
        },
        {
          selector: 'node[status = "down"]',
          style: { 'border-color': '#ef4444', 'border-width': 4 },
        },
        {
          selector: 'node[kind = "cluster"]',
          style: {
            shape: 'round-rectangle',
            'background-opacity': 0.12,
            'border-color': '#f59e0b',
            'border-width': 2,
            'text-valign': 'top',
            'text-halign': 'center',
            'font-size': 16,
            padding: '24px',
          },
        },
        {
          selector: 'edge',
          style: {
            width: 2,
            'line-color': '#64748b',
            'target-arrow-color': '#64748b',
            'target-arrow-shape': 'triangle',
            'curve-style': 'bezier',
            label: 'data(label)',
            'font-size': 10,
            color: '#94a3b8',
          },
        },
        {
          selector: 'edge[style = "dashed"]',
          style: { 'line-style': 'dashed' },
        },
        {
          selector: ':selected',
          style: { 'border-color': '#facc15', 'border-width': 4 },
        },
      ],
      layout: fcoseLayout({ randomize: true, nodeSeparation: 120 }),
      wheelSensitivity: 0.2,
    });

    cy.on('tap', 'node', (evt) => {
      const id = evt.target.id();
      const kind = evt.target.data('kind');
      if (kind !== 'cluster' && onSelect) {
        onSelect(id);
      }
    });
  });

  onDestroy(() => {
    cy?.destroy();
    cy = null;
  });

  // Reactive diff: when nodes/edges change, sync cytoscape's element set
  // without destroying the graph (preserves pan/zoom + animation continuity).
  $effect(() => {
    if (!cy) return;
    const desired = buildElements(nodes, edges);
    const desiredIds = new Set(desired.map((d) => d.data.id as string));
    const existingIds = new Set(cy.elements().map((el) => el.id()));

    const toRemove = cy.elements().filter((el) => !desiredIds.has(el.id()));
    if (toRemove.length > 0) cy.remove(toRemove);

    const toAdd = desired.filter((d) => !existingIds.has(d.data.id as string));
    if (toAdd.length > 0) {
      cy.add(toAdd);
      cy.layout(fcoseLayout({ randomize: false, nodeSeparation: 120 })).run();
    }
  });
</script>

<div bind:this={container} class="topo-canvas"></div>

<style>
  .topo-canvas {
    width: 100%;
    height: calc(100vh - 200px);
    min-height: 600px;
    background: var(--color-surface-1, #0f172a);
    border-radius: var(--radius-md, 8px);
    border: 1px solid var(--color-border, #1e293b);
  }
</style>
