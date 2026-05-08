<script lang="ts">
  import { cx } from '../utils/utils';
  import { getOutgoingEdges, getIncomingEdges } from '../utils/graph';

  interface Props {
    selectedId: string | null;
    selectedNode: TableNode | null;
    edges: Edge[];
    nodeMap: Record<string, TableNode>;
    onclose: () => void;
    ongoto: (id: string) => void;
  }
  let { selectedId, selectedNode, edges, nodeMap, onclose, ongoto }: Props = $props();

  const domainInfo = $derived(selectedNode?.domain ?? { color: '#556', label: 'Other' });
  const outgoingEdges = $derived(selectedId ? getOutgoingEdges(edges, selectedId) : []);
  const incomingEdges = $derived(selectedId ? getIncomingEdges(edges, selectedId) : []);
</script>

<div id="detail" class={selectedId ? 'open' : ''}>
  <div id="detail-header">
    <div>
      <h2>{selectedId ?? ''}</h2>
      {#if selectedNode}
        <span class="domain-badge" style="background: {domainInfo.color}22; color: {domainInfo.color}">
          {domainInfo.label}
        </span>
      {/if}
    </div>
    <button id="detail-close" onclick={onclose}>&times;</button>
  </div>

  {#if selectedNode}
    <div id="detail-columns">
      {#each selectedNode.table.columns as col (col.name)}
        {@const isClickable = !!(col.fk && col.fkTarget && nodeMap[col.fkTarget])}
        <div
          class={cx('detail-col', col.fk && 'fk-row')}
          onclick={isClickable ? () => ongoto(col.fkTarget!) : undefined}
          role={isClickable ? 'button' : undefined}
          tabindex={isClickable ? 0 : undefined}
          onkeydown={isClickable ? (e) => { if (e.key === 'Enter') ongoto(col.fkTarget!); } : undefined}
        >
          <span class={cx('col-badge-d', col.pk && 'pk', col.fk && 'fk')} style={!col.pk && !col.fk ? 'opacity: 0' : ''}>
            {col.pk ? 'PK' : col.fk ? 'FK' : '--'}
          </span>
          <span class="col-name-d">{col.name}</span>
          <span class="col-type-d">{col.type}</span>
          {#if col.fk && col.fkTarget}
            <span class="col-arrow">&rarr; {col.fkTarget}</span>
          {/if}
        </div>
      {/each}
    </div>

    <div id="detail-relations">
      <h3>Relationships</h3>
      <div id="detail-rels-list">
        {#each outgoingEdges as edge (edge.col + edge.target.id)}
          <div class="relation-item" onclick={() => ongoto(edge.target.id)} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Enter') ongoto(edge.target.id); }}>
            <span class="relation-dir">&rarr;</span>
            <span class="relation-table">{edge.target.id}</span>
            <span class="relation-via">via {edge.col}</span>
          </div>
        {/each}
        {#each incomingEdges as edge (edge.col + edge.source.id)}
          <div class="relation-item" onclick={() => ongoto(edge.source.id)} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Enter') ongoto(edge.source.id); }}>
            <span class="relation-dir">&larr;</span>
            <span class="relation-table">{edge.source.id}</span>
            <span class="relation-via">via {edge.col}</span>
          </div>
        {/each}
      </div>
    </div>
  {/if}
</div>
