<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import cytoscape from 'cytoscape';
  import type { Core, ElementDefinition } from 'cytoscape';
  import type { TopologyNode, TopologyEdge, NodeKind, EdgeKind } from '$lib/client/types.gen';

  type Props = {
    nodes: TopologyNode[];
    edges: TopologyEdge[];
    onSelect?: (id: string) => void;
  };

  let { nodes, edges, onSelect }: Props = $props();

  let container: HTMLDivElement;
  let cy: Core | null = null;

  const KIND_GLYPH: Record<NodeKind, string> = {
    host: '🖥',
    vm: '📦',
    lxc: '📦',
    container: '🐳',
    internet: '☁',
    cluster: '🌐',
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
          label: n.kind === 'cluster' ? n.label : `${KIND_GLYPH[n.kind]}  ${n.label}`,
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

  function layout() {
    return {
      name: 'breadthfirst',
      directed: true,
      grid: true,
      spacingFactor: 1.2,
      padding: 24,
      animate: true,
      roots: cy
        ?.nodes()
        .filter((n) => n.data('kind') === 'host' && !n.parent().length)
        .map((n) => n.id()),
    } as cytoscape.LayoutOptions;
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
            shape: 'round-rectangle',
            'background-color': 'data(color)',
            'background-opacity': 0.85,
            color: '#fff',
            'font-size': 13,
            'font-weight': 600,
            'text-valign': 'center',
            'text-halign': 'center',
            'text-outline-width': 2,
            'text-outline-color': '#0f172a',
            'text-wrap': 'wrap',
            width: 'label',
            height: 'label',
            padding: '14px',
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
            'background-opacity': 0.08,
            'border-color': '#f59e0b',
            'border-width': 2,
            'text-valign': 'top',
            'text-halign': 'center',
            'font-size': 15,
            'font-weight': 700,
            color: '#fbbf24',
            'text-outline-width': 0,
            padding: '28px',
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
      wheelSensitivity: 0.2,
    });
    cy.layout(layout()).run();

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
      cy.layout(layout()).run();
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
