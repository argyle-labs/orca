<script lang="ts">
  import type { Snippet } from 'svelte';
  import ProgressBar from './ProgressBar.svelte';

  interface Props {
    label: string;
    labelTitle?: string;
    valueText: string;
    pct: number;
    valueClass?: string;
    extra?: Snippet;
  }
  let { label, labelTitle, valueText, pct, valueClass = '', extra }: Props = $props();
</script>

<div class="metric-row">
  <div class="metric-head">
    <span class="metric-label" title={labelTitle}>{label}</span>
    <span class="metric-val {valueClass}">
      {valueText}
      {#if extra}{@render extra()}{/if}
    </span>
  </div>
  <ProgressBar value={pct} />
</div>

<style>
  .metric-row {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font-size: var(--text-xs);
  }
  .metric-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 4px;
  }
  .metric-label {
    color: var(--color-text-dim);
    text-transform: uppercase;
    font-size: 10px;
    letter-spacing: 0.06em;
    flex-shrink: 0;
  }
  .metric-val {
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
    font-size: var(--text-xs);
    text-align: right;
  }
  .metric-val :global(.dim) { color: var(--color-text-dim); }
  .metric-val :global(.load-legend) {
    font-size: 9px;
    letter-spacing: 0.04em;
    opacity: 0.7;
  }
  .metric-val.gpu-val {
    display: flex;
    align-items: center;
    gap: 4px;
    justify-content: flex-end;
  }
  .metric-val :global(.gpu-name) {
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
