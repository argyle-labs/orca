<script lang="ts">
  import type { ChartPoint } from '$lib/client/types.gen';

  // Server-shaped chart primitive. The daemon's `system.detail_view` (and
  // future series-emitting tools) returns pre-scaled `(x,y)` points plus
  // gap indices; this component is a thin renderer that walks the points
  // and emits one `M…L…L…` SVG path segment per contiguous run. Native
  // iOS/Android clients can render the same `points`/`gaps` shape with
  // their own primitives — keeps the per-platform code to "draw a line
  // between these N pixel coordinates".

  interface Props {
    label: string;
    points: ChartPoint[];
    gaps: number[];
    vmax: number;
    unit: string;
    lastValue?: number | null;
    width?: number;
    height?: number;
    color?: string;
  }
  let {
    label,
    points,
    gaps,
    vmax,
    unit,
    lastValue = null,
    width = 400,
    height = 90,
    color = '#89b4fa',
  }: Props = $props();

  type Segment = { line: string; area: string };

  // Group points into contiguous segments separated by `gaps` indices.
  // `gaps[i] = N` means break the path between points[N-1] and points[N].
  let segs = $derived.by<Segment[]>(() => {
    if (!points.length) return [];
    const gapSet = new Set(gaps);
    const out: Segment[] = [];
    let line = '';
    let area = '';
    let segStartX: number | null = null;
    let segLastX: number | null = null;
    const flush = () => {
      if (line && segStartX != null && segLastX != null) {
        out.push({
          line: line.trim(),
          area: `${area} L ${segLastX.toFixed(1)} ${height} L ${segStartX.toFixed(1)} ${height} Z`.trim(),
        });
      }
      line = '';
      area = '';
      segStartX = null;
      segLastX = null;
    };
    for (let i = 0; i < points.length; i++) {
      if (gapSet.has(i)) flush();
      const { x, y } = points[i];
      if (line === '') {
        line = `M ${x.toFixed(1)} ${y.toFixed(1)} `;
        area = `M ${x.toFixed(1)} ${y.toFixed(1)} `;
        segStartX = x;
      } else {
        line += `L ${x.toFixed(1)} ${y.toFixed(1)} `;
        area += `L ${x.toFixed(1)} ${y.toFixed(1)} `;
      }
      segLastX = x;
    }
    flush();
    return out;
  });

  let lastStr = $derived(
    lastValue == null || !Number.isFinite(lastValue)
      ? '—'
      : unit === '%'
        ? `${lastValue.toFixed(1)}%`
        : lastValue < 1024
          ? `${Math.round(lastValue)} ${unit}`
          : `${(lastValue / 1024).toFixed(1)} G${unit}`,
  );
  let axisMax = $derived(
    unit === '%' ? '100%' : vmax < 1024 ? `${Math.round(vmax)} ${unit}` : `${(vmax / 1024).toFixed(1)} G${unit}`,
  );
  const GRIDLINES = [0.25, 0.5, 0.75];
</script>

<div class="hist-cell">
  <div class="hist-label">{label}<span class="hist-val">{lastStr}</span></div>
  <svg class="hist-svg" viewBox="0 0 {width} {height}" preserveAspectRatio="none" style="color: {color};">
    {#each GRIDLINES as g}
      <line x1="0" x2={width} y1={height * (1 - g)} y2={height * (1 - g)} stroke="currentColor" stroke-width="0.5" opacity="0.15" />
    {/each}
    {#each segs as s}
      <path d={s.area} fill="currentColor" opacity="0.18" />
      <path d={s.line} fill="none" stroke="currentColor" stroke-width="1.5" />
    {/each}
  </svg>
  <div class="hist-axis"><span>0</span><span>{axisMax}</span></div>
</div>

<style>
  .hist-cell {
    background: var(--bg-elevated, rgba(255, 255, 255, 0.03));
    border: 1px solid var(--border-subtle, rgba(255, 255, 255, 0.06));
    border-radius: 6px;
    padding: 8px;
    color: var(--accent, #89b4fa);
  }
  .hist-label {
    display: flex;
    justify-content: space-between;
    font-size: 11px;
    color: var(--text-secondary, rgba(255, 255, 255, 0.6));
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin-bottom: 4px;
  }
  .hist-val {
    color: var(--text-primary, #fff);
    font-family: ui-monospace, monospace;
    text-transform: none;
    letter-spacing: 0;
  }
  .hist-svg { display: block; width: 100%; height: 90px; }
  .hist-axis {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    color: var(--text-secondary, rgba(255, 255, 255, 0.45));
    font-family: ui-monospace, monospace;
    margin-top: 2px;
  }
</style>
