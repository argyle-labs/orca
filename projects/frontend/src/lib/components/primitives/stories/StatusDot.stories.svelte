<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import StatusDot from '../StatusDot.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/StatusDot',
    component: StatusDot,
    tags: ['autodocs'],
  });
</script>

<Story name="All states">
  {#snippet template()}
    <div style="display:flex; gap:1rem; align-items:center; font-size:0.85rem;">
      <span><StatusDot ok={true} /> up</span>
      <span><StatusDot ok={false} /> down</span>
      <span><StatusDot ok={null} /> unknown</span>
    </div>
  {/snippet}
</Story>
