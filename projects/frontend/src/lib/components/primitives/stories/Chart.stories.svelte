<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Chart from '../Chart.svelte';
  import type { ChartPoint } from '$lib/client/types.gen';

  const { Story } = defineMeta({
    title: 'Primitives/Chart',
    component: Chart,
    tags: ['autodocs'],
  });

  // Build SVG-space points from a synthetic value series. Mirrors what the
  // daemon does in `system.detail_view` — value→y via vmax scaling, even
  // x distribution across width.
  function wavePoints(
    n: number,
    amp: number,
    base: number,
    vmax: number,
    W: number,
    H: number,
    phase = 0,
  ): { points: ChartPoint[]; last: number } {
    const points: ChartPoint[] = [];
    let last = base;
    for (let i = 0; i < n; i++) {
      const v = base + Math.sin((i + phase) * 0.4) * amp + Math.cos((i + phase) * 0.13) * (amp * 0.3);
      const clamped = Math.max(0, Math.min(v, vmax));
      points.push({ x: (i / Math.max(1, n - 1)) * W, y: H - (clamped / vmax) * H });
      last = v;
    }
    return { points, last };
  }

  const cpu = wavePoints(60, 18, 40, 100, 420, 90);
  const net = wavePoints(60, 4_000_000, 6_000_000, 12_000_000, 420, 90, 5);

  // Gap demo — 13 points with a break at index 2 and index 7.
  const memPoints: ChartPoint[] = [20, 22, 28, 30, 31, 35, 38, 36, 34, 30].map((v, i) => ({
    x: (i / 9) * 420,
    y: 90 - (v / 100) * 90,
  }));
</script>

<Story name="CPU (percent)">
  {#snippet template()}
    <div style="width:420px;">
      <Chart label="CPU" points={cpu.points} gaps={[]} vmax={100} unit="%" lastValue={cpu.last} />
    </div>
  {/snippet}
</Story>

<Story name="Network (bytes/s)">
  {#snippet template()}
    <div style="width:420px;">
      <Chart
        label="Net RX"
        points={net.points}
        gaps={[]}
        vmax={12_000_000}
        unit="B/s"
        color="#a6e3a1"
        lastValue={net.last}
      />
    </div>
  {/snippet}
</Story>

<Story name="Sparse / gap data">
  {#snippet template()}
    <div style="width:420px;">
      <Chart
        label="Memory"
        points={memPoints}
        gaps={[2, 7]}
        vmax={100}
        unit="%"
        color="#f9e2af"
        lastValue={30}
      />
    </div>
  {/snippet}
</Story>
