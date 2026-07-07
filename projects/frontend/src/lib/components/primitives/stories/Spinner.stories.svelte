<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Spinner from '../Spinner.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Spinner',
    component: Spinner,
    tags: ['autodocs'],
    argTypes: { size: { control: { type: 'range', min: 8, max: 64, step: 1 } } },
    args: { size: 16 },
  });
</script>

<Story name="Default">
  {#snippet template(args)}
    <Spinner {...args} />
  {/snippet}
</Story>

<Story name="Sizes">
  {#snippet template()}
    <div style="display:flex; gap:1rem; align-items:center;">
      <Spinner size={12} />
      <Spinner size={16} />
      <Spinner size={24} />
      <Spinner size={40} />
    </div>
  {/snippet}
</Story>
