<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Chart from '../Chart.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Chart',
    component: Chart,
    tags: ['autodocs'],
  });

  function wave(n: number, amp: number, base: number, phase = 0): number[] {
    return Array.from({ length: n }, (_, i) =>
      base + Math.sin((i + phase) * 0.4) * amp + Math.cos((i + phase) * 0.13) * (amp * 0.3),
    );
  }
</script>

<Story name="CPU (percent)">
  {#snippet template()}
    <div style="width:420px;">
      <Chart label="CPU" vals={wave(60, 18, 40)} vmax={100} unit="%" />
    </div>
  {/snippet}
</Story>

<Story name="Network (bytes/s)">
  {#snippet template()}
    <div style="width:420px;">
      <Chart label="Net RX" vals={wave(60, 4_000_000, 6_000_000, 5)} vmax={12_000_000} unit="B/s" color="#a6e3a1" />
    </div>
  {/snippet}
</Story>

<Story name="Sparse / gap data">
  {#snippet template()}
    <div style="width:420px;">
      <Chart
        label="Memory"
        vals={[20, 22, NaN, NaN, 28, 30, 31, NaN, 35, 38, 36, 34, 30]}
        vmax={100}
        unit="%"
        color="#f9e2af"
      />
    </div>
  {/snippet}
</Story>
