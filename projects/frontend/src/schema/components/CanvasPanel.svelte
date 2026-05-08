<script lang="ts">
  import { onMount } from 'svelte';
  import { computeLayout } from '../utils/layout';
  import { buildDomainMap } from '../utils/utils';
  import { createCamera } from '../hooks/useCamera.svelte';
  import { createCardDrag } from '../hooks/useCardDrag.svelte';
  import { attachKeyboardShortcuts } from '../hooks/useKeyboardShortcuts.svelte';
  import EdgesSvg from './EdgesSvg.svelte';
  import GroupBackgrounds from './GroupBackgrounds.svelte';
  import TableCard from './TableCard.svelte';
  import DetailPanel from './DetailPanel.svelte';
  import ZoomControls from './ZoomControls.svelte';
  import Toast from './Toast.svelte';
  import DriftPanel from './DriftPanel.svelte';

  interface Props {
    data: TabData;
    searchQuery: string;
    searchMatchSet: Set<string> | null;
    activeDomains: Set<string>;
    selected: string | null;
    onselect: (val: string | ((prev: string | null) => string | null)) => void;
    ongoto: (id: string) => void;
    drift?: DriftReport;
  }
  let { data, searchMatchSet, activeDomains, selected, onselect, ongoto, drift }: Props = $props();

  // Layout is computed once per CanvasPanel mount (data is keyed in parent).
  // svelte-ignore state_referenced_locally
  const initialData = data;
  const layout = computeLayout(initialData.tables, initialData.fks, initialData.domains);

  let hovered = $state<string | null>(null);
  let viewportEl: HTMLDivElement | undefined = $state();
  let worldEl: HTMLDivElement | undefined = $state();
  // Bumped when card drag finishes to force reactive re-render of card positions.
  let dragVersion = $state(0);

  const camera = createCamera({
    getViewport: () => viewportEl,
    getWorld: () => worldEl,
    wW: layout.wW,
    wH: layout.wH,
    onBackgroundClick: () => onselect(''),
  });

  const cardDrag = createCardDrag({
    getViewport: () => viewportEl,
    cam: camera.cam,
    edges: layout.edges,
    focusNode: camera.focusNode,
    setSelected: (v) => onselect(v),
    bumpVersion: () => (dragVersion = dragVersion + 1),
  });

  const domainOf = buildDomainMap(initialData.domains);

  function isDimmed(id: string): boolean {
    const domain = domainOf[id];
    if (domain && !activeDomains.has(domain.key)) return true;
    if (searchMatchSet && !searchMatchSet.has(id)) return true;
    return selected ? id !== selected && !layout.adjT[selected]?.has(id) : false;
  }

  function focusAndSelect(id: string) {
    onselect(id);
    if (layout.nodeMap[id]) camera.focusNode(layout.nodeMap[id]);
    ongoto(id);
  }

  // Attach DOM event handlers once mounted; detach on unmount.
  onMount(() => {
    const detachCam = camera.attach();
    const detachKeys = attachKeyboardShortcuts({ onClear: () => onselect('') });
    return () => {
      detachCam();
      detachKeys();
    };
  });

  const selectedNode = $derived(selected ? layout.nodeMap[selected] : null);
</script>

<div id="viewport" bind:this={viewportEl}>
  <div id="world" bind:this={worldEl}>
    <EdgesSvg edges={layout.edges} width={layout.wW} height={layout.wH} {isDimmed} selectedId={selected} hoveredId={hovered} />
    <GroupBackgrounds groups={layout.groups} />
    {#key dragVersion}
      {#each layout.nodes as node (node.id)}
        {@const dim = isDimmed(node.id)}
        {@const isConnected = !selected && !dim && !!(hovered && layout.adjT[hovered]?.has(node.id))}
        <TableCard
          {node}
          isDimmed={dim}
          isSelected={selected === node.id}
          {isConnected}
          isSearchMatch={!!searchMatchSet?.has(node.id)}
          onhover={() => (hovered = node.id)}
          onleave={() => (hovered = null)}
          onpointerdownnode={(e) => cardDrag.handleCardPointerDown(e, node)}
          onpointermovenode={(e) => cardDrag.handleCardPointerMove(e, node)}
          onpointerupnode={(e) => cardDrag.handleCardPointerUp(e, node)}
        />
      {/each}
    {/key}
  </div>

  <DetailPanel
    selectedId={selected}
    {selectedNode}
    edges={layout.edges}
    nodeMap={layout.nodeMap}
    onclose={() => onselect('')}
    ongoto={focusAndSelect}
  />
  <ZoomControls onzoomin={() => camera.zoomBy(1.4)} onzoomout={() => camera.zoomBy(1 / 1.4)} onfitall={() => camera.fitAll(true)} />
  <Toast />
  {#if drift}<DriftPanel {drift} ongoto={focusAndSelect} />{/if}
</div>
