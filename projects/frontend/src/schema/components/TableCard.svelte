<script lang="ts">
  import { MAX_VISIBLE_COLS } from '../utils/constants';
  import { cx } from '../utils/utils';

  interface Props {
    node: TableNode;
    isDimmed: boolean;
    isSelected: boolean;
    isConnected: boolean;
    isSearchMatch: boolean;
    onhover: () => void;
    onleave: () => void;
    onpointerdownnode: (e: PointerEvent) => void;
    onpointermovenode: (e: PointerEvent) => void;
    onpointerupnode: (e: PointerEvent) => void;
  }
  let {
    node, isDimmed, isSelected, isConnected, isSearchMatch,
    onhover, onleave, onpointerdownnode, onpointermovenode, onpointerupnode,
  }: Props = $props();

  const domainColor = $derived(node.domain?.color ?? '#556');
</script>

<div
  class={cx('table-card', isDimmed && 'dim', isSelected && 'selected', isConnected && 'connected', isSearchMatch && 'search-match')}
  data-table={node.id}
  style="left: {node.x}px; top: {node.y}px"
  onmouseenter={onhover}
  onmouseleave={onleave}
  onpointerdown={onpointerdownnode}
  onpointermove={onpointermovenode}
  onpointerup={onpointerupnode}
  role="button"
  tabindex="0"
>
  <div class="table-header" style="background: {domainColor}">
    <span class="name">{node.id}</span>
    <span class="count">{node.table.columns.length} cols</span>
  </div>
  <div class="table-cols">
    {#each node.table.columns.slice(0, MAX_VISIBLE_COLS) as col (col.name)}
      <div class="table-col">
        <span class={cx('col-badge', col.pk ? 'pk' : col.fk ? 'fk' : 'none')}>{col.pk ? 'PK' : col.fk ? 'FK' : '--'}</span>
        <span class={cx('col-name', col.pk && 'is-pk', col.fk && 'is-fk')}>{col.name}</span>
        <span class="col-type">{col.type}</span>
      </div>
    {/each}
  </div>
  {#if node.table.columns.length > MAX_VISIBLE_COLS}
    <div class="table-more">+ {node.table.columns.length - MAX_VISIBLE_COLS} more</div>
  {/if}
</div>
