<script lang="ts">
  import TgCard from './_TgCard.svelte';

  interface Props {
    domain: Domain;
    tables: Table[];
    selected: string | null;
    onselect: (id: string) => void;
    fkOutCount: Record<string, number>;
    fkInCount: Record<string, number>;
  }
  let { domain, tables, selected, onselect, fkOutCount, fkInCount }: Props = $props();

  let collapsed = $state(false);
</script>

<section class="tg-section">
  <button class="tg-section-header" onclick={() => (collapsed = !collapsed)} style="border-color: {domain.color}">
    <span class="tg-section-dot" style="background: {domain.color}"></span>
    <span class="tg-section-label">{domain.label}</span>
    <span class="tg-section-count">{tables.length}</span>
    <span class="tg-section-chevron">{collapsed ? '▸' : '▾'}</span>
  </button>

  {#if !collapsed}
    <div class="tg-grid">
      {#each tables as table (table.name)}
        <TgCard
          {table}
          {domain}
          isSelected={selected === table.name}
          fkOut={fkOutCount[table.name] ?? 0}
          fkIn={fkInCount[table.name] ?? 0}
          {onselect}
        />
      {/each}
    </div>
  {/if}
</section>
