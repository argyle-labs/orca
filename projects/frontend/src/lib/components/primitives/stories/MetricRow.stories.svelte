<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import MetricRow from '../MetricRow.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/MetricRow',
    component: MetricRow,
    tags: ['autodocs'],
    args: { label: 'CPU', valueText: '42%', pct: 42 },
  });
</script>

<Story name="Default">
  {#snippet template({ extra: _e, ...args })}
    <div style="width:280px;">
      <MetricRow {...args} />
    </div>
  {/snippet}
</Story>

<Story name="Host snapshot">
  {#snippet template()}
    <div style="display:flex; flex-direction:column; gap:0.75rem; width:280px;">
      <MetricRow label="CPU" valueText="38.4%" pct={38.4} />
      <MetricRow label="Memory" valueText="7.2 / 16 GiB" pct={45} />
      <MetricRow label="Disk" valueText="78%" pct={78} />
      <MetricRow label="Swap" valueText="94%" pct={94} />
    </div>
  {/snippet}
</Story>
