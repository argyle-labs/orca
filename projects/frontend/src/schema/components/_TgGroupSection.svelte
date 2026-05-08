<script lang="ts">
  import DomainSection from './_TgDomainSection.svelte';
  import type { GroupEntry } from './TableGrid.svelte';

  interface Props {
    group: GroupEntry;
    selected: string | null;
    onselect: (id: string) => void;
    fkOutCount: Record<string, number>;
    fkInCount: Record<string, number>;
  }
  let { group, selected, onselect, fkOutCount, fkInCount }: Props = $props();

  let collapsed = $state(false);
</script>

<section class="tg-group">
  <button class="tg-group-header" onclick={() => (collapsed = !collapsed)}>
    <span class="tg-group-label">{group.label}</span>
    <span class="tg-group-count">{group.total}</span>
    <span class="tg-group-chevron">{collapsed ? '▸' : '▾'}</span>
  </button>

  {#if !collapsed}
    <div class="tg-group-body">
      {#each group.subgroups as sg (sg.key)}
        <div class={sg.label ? 'tg-subgroup' : undefined}>
          {#if sg.label}<div class="tg-subgroup-header">{sg.label}</div>{/if}
          {#each sg.entries as { domain, tables } (domain.key)}
            <DomainSection {domain} {tables} {selected} {onselect} {fkOutCount} {fkInCount} />
          {/each}
        </div>
      {/each}
    </div>
  {/if}
</section>
