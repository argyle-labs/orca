<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Drawer from '../Drawer.svelte';
  import Button from '../Button.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Drawer',
    component: Drawer,
    tags: ['autodocs'],
  });
</script>

<script lang="ts">
  let openRight = $state(false);
  let openLeft = $state(false);
</script>

<Story name="Right">
  {#snippet template()}
    <Button variant="primary" onclick={() => (openRight = true)}>Open right drawer</Button>
    <Drawer open={openRight} side="right" onclose={() => (openRight = false)} ariaLabel="Demo drawer">
      <div style="padding:1rem;">
        <h3 style="margin-top:0;">Drawer content</h3>
        <p>Escape, backdrop click, or the close handler dismiss it.</p>
        <Button onclick={() => (openRight = false)}>Close</Button>
      </div>
    </Drawer>
  {/snippet}
</Story>

<Story name="Left">
  {#snippet template()}
    <Button variant="primary" onclick={() => (openLeft = true)}>Open left drawer</Button>
    <Drawer open={openLeft} side="left" onclose={() => (openLeft = false)} ariaLabel="Demo drawer left">
      <div style="padding:1rem;">
        <p>Left side drawer.</p>
        <Button onclick={() => (openLeft = false)}>Close</Button>
      </div>
    </Drawer>
  {/snippet}
</Story>
