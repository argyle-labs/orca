<script lang="ts">
  import { chartSegments } from '$lib/utils/chart';

  interface Props {
    label: string;
    vals: number[];
    vmax: number;
    unit: string;
    color?: string;
    width?: number;
    height?: number;
  }
  let {
    label,
    vals,
    vmax,
    unit,
    color = '#89b4fa',
    width = 400,
    height = 90,
  }: Props = $props();

  let segs = $derived(chartSegments(vals, width, height, vmax));
  let last = $derived([...vals].reverse().find(Number.isFinite) ?? null);
  let lastStr = $derived(
    last == null
      ? '—'
      : unit === '%'
        ? `${last.toFixed(1)}%`
        : last < 1024
          ? `${Math.round(last)} ${unit}`
          : `${(last / 1024).toFixed(1)} G${unit}`,
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
