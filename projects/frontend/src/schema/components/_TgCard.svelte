<script lang="ts">
  import { cx } from '../utils/utils';

  interface Props {
    table: Table;
    domain: Domain;
    isSelected: boolean;
    fkOut: number;
    fkIn: number;
    onselect: (id: string) => void;
  }
  let { table, domain, isSelected, fkOut, fkIn, onselect }: Props = $props();

  const pkCols = $derived(table.columns.filter((c) => c.pk));
  const fkCols = $derived(table.columns.filter((c) => c.fk));
</script>

<button
  class={cx('tg-card', isSelected && 'tg-card-selected')}
  style="--domain-color: {domain.color}"
  onclick={() => onselect(table.name)}
>
  <div class="tg-card-header">
    <span class="tg-card-name">{table.name}</span>
    <span class="tg-card-cols">{table.columns.length} cols</span>
  </div>
  <div class="tg-card-meta">
    {#if pkCols.length > 0}<span class="tg-badge tg-badge-pk">PK·{pkCols.length}</span>{/if}
    {#if fkCols.length > 0}<span class="tg-badge tg-badge-fk">FK·{fkCols.length}</span>{/if}
    {#if fkIn > 0}<span class="tg-badge tg-badge-ref">←{fkIn}</span>{/if}
  </div>
</button>
