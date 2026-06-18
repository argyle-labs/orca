<script lang="ts">
  import ClusterHeader from '$lib/components/ClusterHeader.svelte';
  import InstanceCard from '$lib/components/InstanceCard.svelte';
  import InstanceTreeRow from '$lib/components/InstanceTreeRow.svelte';
  import type { DisplayRow } from '$lib/types/instance';

  interface Props {
    rows: DisplayRow[];
    view: 'tree' | 'table';
    collapsed: Set<string>;
    onactivate: (peerId: string) => void;
    ontoggle: (peerId: string) => void;
  }
  let { rows, view, collapsed, onactivate, ontoggle }: Props = $props();
</script>

<div class="instances" class:tree={view === 'tree'}>
  {#each rows as row (row.key)}
    {#if row.kind === 'header'}
      <ClusterHeader cluster={row.cluster} summary={row.summary} />
    {:else}
      {@const inst = row.inst}
      {#if view === 'tree'}
        <InstanceTreeRow
          {inst}
          prefix={row.prefix}
          hasChildren={row.hasChildren}
          collapsed={collapsed.has(inst.peerId)}
          onactivate={() => onactivate(inst.peerId)}
          ontoggle={() => ontoggle(inst.peerId)}
        />
      {:else}
        <InstanceCard {inst} depth={row.depth} onactivate={() => onactivate(inst.peerId)} />
      {/if}
    {/if}
  {/each}
</div>

<style>
  .instances {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
    gap: var(--space-4);
  }
  .instances.tree {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
</style>
