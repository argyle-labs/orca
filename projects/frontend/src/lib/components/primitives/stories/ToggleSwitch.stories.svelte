<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import ToggleSwitch from '../ToggleSwitch.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/ToggleSwitch',
    component: ToggleSwitch,
    tags: ['autodocs'],
    args: { checked: false, disabled: false, ariaLabel: 'Demo toggle' },
  });
</script>

<script lang="ts">
  let on = $state(false);
  let off = $state(true);
</script>

<Story name="Off">
  {#snippet template(args)}
    <ToggleSwitch {...args} checked={on} onchange={() => (on = !on)} />
  {/snippet}
</Story>

<Story name="On" args={{ checked: true }}>
  {#snippet template(args)}
    <ToggleSwitch {...args} checked={off} onchange={() => (off = !off)} />
  {/snippet}
</Story>

<Story name="Disabled" args={{ disabled: true }}>
  {#snippet template(args)}
    <div style="display:flex; gap:0.75rem;">
      <ToggleSwitch checked={false} disabled onchange={() => {}} ariaLabel="off disabled" />
      <ToggleSwitch checked={true} disabled onchange={() => {}} ariaLabel="on disabled" />
    </div>
  {/snippet}
</Story>
