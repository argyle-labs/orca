<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Modal from '../Modal.svelte';
  import Button from '../Button.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Modal',
    component: Modal,
    tags: ['autodocs'],
  });
</script>

<script lang="ts">
  let openSm = $state(false);
  let openMd = $state(false);
  let openLg = $state(false);
</script>

<Story name="Small">
  {#snippet template()}
    <Button variant="primary" onclick={() => (openSm = true)}>Open small</Button>
    <Modal open={openSm} title="Confirm" size="sm" onclose={() => (openSm = false)}>
      <p>Proceed with this action?</p>
      <div style="display:flex; gap:0.5rem; justify-content:flex-end;">
        <Button onclick={() => (openSm = false)}>Cancel</Button>
        <Button variant="primary" onclick={() => (openSm = false)}>OK</Button>
      </div>
    </Modal>
  {/snippet}
</Story>

<Story name="Medium">
  {#snippet template()}
    <Button variant="primary" onclick={() => (openMd = true)}>Open medium</Button>
    <Modal open={openMd} title="Edit configuration" size="md" onclose={() => (openMd = false)}>
      <p>Body content goes here. Modal wraps a native &lt;dialog&gt; with backdrop, escape-to-close, and a header.</p>
    </Modal>
  {/snippet}
</Story>

<Story name="Large">
  {#snippet template()}
    <Button variant="primary" onclick={() => (openLg = true)}>Open large</Button>
    <Modal open={openLg} title="Detail view" size="lg" onclose={() => (openLg = false)}>
      <div style="height:300px; display:flex; align-items:center; justify-content:center; color:var(--color-text-dim);">
        Large modal body placeholder
      </div>
    </Modal>
  {/snippet}
</Story>
