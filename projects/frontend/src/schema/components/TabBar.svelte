<script lang="ts">
  import { cx } from '../utils/utils';

  interface Props {
    tabs: { title: string }[];
    activeIndex: number;
    onselect: (index: number) => void;
  }
  let { tabs, activeIndex, onselect }: Props = $props();
</script>

<div id="tab-bar">
  {#each tabs as tab, i (i)}
    <button class={cx('tab-item', i === activeIndex && 'active')} onclick={() => onselect(i)}>
      {tab.title}
    </button>
  {/each}
</div>
