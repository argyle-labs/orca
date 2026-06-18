<script lang="ts">
  import StatusDot from './primitives/StatusDot.svelte';
  import MetricRow from './primitives/MetricRow.svelte';
  import { cpuPct, memPct, loadPct } from '$lib/utils/sysMetrics';
  import { fmtMb } from '$lib/utils/format';
  import type { Instance } from '$lib/types/instance';

  let {
    inst,
    depth = 0,
    onactivate,
  }: {
    inst: Instance;
    depth?: number;
    onactivate: () => void;
  } = $props();
</script>

<div
  class="instance"
  class:down={inst.health === 'down'}
  class:child={depth > 0}
  style:margin-left="{depth * 24}px"
  onclick={onactivate}
  role="button"
  tabindex="0"
  onkeydown={(e) => e.key === 'Enter' && onactivate()}
>
  <div class="card-header">
    <div class="ident">
      <StatusDot ok={inst.health === 'up' ? true : inst.health === 'down' ? false : null} />
      <span class="hostname">{inst.sys?.hostname ?? inst.label}</span>
      {#if inst.updateAvailable}
        <span class="update-badge" title="Update available: {inst.updateLatest ?? 'newer version'}">↑ {inst.updateLatest ?? 'update'}</span>
      {/if}
    </div>
  </div>

  {#if inst.sys}
    {@const sys = inst.sys}
    {@const cp = cpuPct(sys)}
    {@const mp = memPct(sys)}
    {@const lp = loadPct(sys)}
    <div class="metrics">
      <MetricRow
        label="CPU"
        valueText={cp != null ? `${cp.toFixed(1)}%` : '—'}
        pct={cp ?? 0}
      />
      <MetricRow label="RAM" valueText={`${mp.toFixed(1)}%`} pct={mp}>
        {#snippet extra()}
          <span class="dim">{fmtMb(sys.mem_used_mb)} / {fmtMb(sys.mem_total_mb)}</span>
        {/snippet}
      </MetricRow>
      {#if sys.load_avg_1 != null}
        <MetricRow
          label="CPU Q"
          labelTitle="Unix run-queue depth (processes waiting for CPU), normalized by core count"
          valueText={sys.load_avg_1.toFixed(2)}
          pct={lp ?? 0}
        >
          {#snippet extra()}
            <span class="dim">/{sys.cpu_logical ?? '?'} <span class="load-legend">1m avg</span></span>
          {/snippet}
        </MetricRow>
      {/if}
      {#if sys.gpus?.length}
        {#each sys.gpus as g}
          <MetricRow
            label="GPU"
            valueClass="gpu-val"
            valueText={g.utilization_percent != null ? `${g.utilization_percent.toFixed(0)}%` : '—'}
            pct={g.utilization_percent ?? 0}
          >
            {#snippet extra()}
              <span class="dim gpu-name">{g.name}</span>
            {/snippet}
          </MetricRow>
        {/each}
      {/if}
    </div>
  {/if}

  <div class="card-footer">
    <span class="details-hint">Details →</span>
  </div>
</div>

<style>
  .instance {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    cursor: pointer;
    transition: border-color 0.15s ease;
    text-align: left;
  }
  .instance:hover { border-color: var(--color-accent, #4f86f7); }
  .instance.down { border-color: var(--color-error); }
  .instance:focus-visible {
    outline: 2px solid var(--color-accent, #4f86f7);
    outline-offset: 2px;
  }
  .card-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .ident {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .hostname { font-weight: var(--weight-semibold); }
  .update-badge {
    font-size: var(--text-xs);
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 999px;
    background: color-mix(in srgb, #f59e0b 25%, transparent);
    color: #f5a623;
    border: 1px solid color-mix(in srgb, #f59e0b 60%, transparent);
    white-space: nowrap;
    flex-shrink: 0;
  }
  .metrics {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  .dim { color: var(--color-text-dim); }
  .card-footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
    gap: var(--space-2);
    margin-top: auto;
  }
  .details-hint {
    flex-shrink: 0;
    color: var(--color-accent, #4f86f7);
    font-size: 10px;
    letter-spacing: 0.04em;
  }
</style>
