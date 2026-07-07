<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import IconButton from '../IconButton.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/IconButton',
    component: IconButton,
    tags: ['autodocs'],
    args: { title: 'Close', disabled: false },
  });
</script>

<Story name="Close">
  {#snippet template({ children: _c, ...args })}
    <IconButton {...args}>✕</IconButton>
  {/snippet}
</Story>

<Story name="Toolbar">
  {#snippet template()}
    <div style="display:flex; gap:0.25rem;">
      <IconButton title="Refresh">↻</IconButton>
      <IconButton title="Settings">⚙</IconButton>
      <IconButton title="Search">⌕</IconButton>
      <IconButton title="Close">✕</IconButton>
    </div>
  {/snippet}
</Story>

<Story name="Disabled" args={{ disabled: true }}>
  {#snippet template({ children: _c, ...args })}
    <IconButton {...args}>✕</IconButton>
  {/snippet}
</Story>
