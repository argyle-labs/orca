<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Popover from '../Popover.svelte';
  import Button from '../Button.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Popover',
    component: Popover,
    tags: ['autodocs'],
  });
</script>

<script lang="ts">
  let openStart = $state(false);
  let openEnd = $state(false);
</script>

<Story name="Align start">
  {#snippet template()}
    <div style="padding:3rem;">
      <Popover bind:open={openStart} align="start" width={220}>
        {#snippet trigger()}
          <Button onclick={() => (openStart = !openStart)}>Open popover</Button>
        {/snippet}
        {#snippet children()}
          <div style="padding:0.75rem;">
            <p style="margin:0 0 0.5rem;">Popover content.</p>
            <Button size="sm" onclick={() => (openStart = false)}>Close</Button>
          </div>
        {/snippet}
      </Popover>
    </div>
  {/snippet}
</Story>

<Story name="Align end">
  {#snippet template()}
    <div style="padding:3rem; display:flex; justify-content:flex-end;">
      <Popover bind:open={openEnd} align="end" width={220}>
        {#snippet trigger()}
          <Button onclick={() => (openEnd = !openEnd)}>Right-aligned</Button>
        {/snippet}
        {#snippet children()}
          <div style="padding:0.75rem;">Aligned to the trigger's right edge.</div>
        {/snippet}
      </Popover>
    </div>
  {/snippet}
</Story>
