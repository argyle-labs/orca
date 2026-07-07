<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import ProgressBar from '../ProgressBar.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/ProgressBar',
    component: ProgressBar,
    tags: ['autodocs'],
    argTypes: {
      value: { control: { type: 'range', min: 0, max: 100, step: 1 } },
      warn: { control: { type: 'range', min: 0, max: 100, step: 1 } },
      crit: { control: { type: 'range', min: 0, max: 100, step: 1 } },
    },
    args: { value: 40, warn: 70, crit: 90 },
  });
</script>

<Story name="Default">
  {#snippet template(args)}
    <div style="width:240px;">
      <ProgressBar {...args} />
    </div>
  {/snippet}
</Story>

<Story name="Thresholds">
  {#snippet template()}
    <div style="display:flex; flex-direction:column; gap:0.5rem; width:240px;">
      <ProgressBar value={20} />
      <ProgressBar value={75} />
      <ProgressBar value={95} />
    </div>
  {/snippet}
</Story>
