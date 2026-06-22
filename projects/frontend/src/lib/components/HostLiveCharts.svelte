<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import Chart from '$lib/components/primitives/Chart.svelte';
  import SectionHead from '$lib/components/primitives/SectionHead.svelte';
  import { systemDetailView } from '$lib/client/sdk.gen';
  import { unwrap, peerHeader } from '$lib/stores/runTool';
  import type { PodInstance, SystemDetailViewResponses } from '$lib/client/types.gen';

  interface Props {
    inst: PodInstance;
  }
  let { inst }: Props = $props();

  type View = SystemDetailViewResponses[200];

  const W = 420;
  const H = 90;
  const POLL_MS = 5000;

  let view = $state<View | null>(null);
  let pollHandle: ReturnType<typeof setInterval> | null = null;
  let lastId = $state<string | null>(null);

  async function refresh() {
    try {
      const target = inst.role === 'local' ? null : inst.peer_id;
      const r = await unwrap(
        systemDetailView({
          body: { width: W, height: H },
          headers: peerHeader(target),
        }),
      );
      view = r;
    } catch (e) {
      console.warn('systemDetailView failed', e);
    }
  }

  $effect(() => {
    if (inst.id !== lastId) {
      view = null;
      lastId = inst.id;
      void refresh();
    }
  });

  onMount(() => {
    void refresh();
    pollHandle = setInterval(refresh, POLL_MS);
  });
  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });
</script>

<SectionHead title="Live">
  {#snippet trailing()}
    <span class="section-meta">{view?.samples_count ?? 0} samples</span>
  {/snippet}
</SectionHead>
<div class="hist-grid">
  {#if view}
    <Chart
      label="CPU"
      points={view.cpu.points}
      gaps={view.cpu.gaps}
      vmax={view.cpu.vmax}
      lastValue={view.cpu.last_value}
      unit="%"
      color="#89b4fa"
      width={W}
      height={H}
    />
    <Chart
      label="RAM"
      points={view.mem.points}
      gaps={view.mem.gaps}
      vmax={view.mem.vmax}
      lastValue={view.mem.last_value}
      unit="%"
      color="#a6e3a1"
      width={W}
      height={H}
    />
    {#each view.gpus as g}
      <Chart
        label={g.name}
        points={g.utilization.points}
        gaps={g.utilization.gaps}
        vmax={g.utilization.vmax}
        lastValue={g.utilization.last_value}
        unit="%"
        color="#f5c2e7"
        width={W}
        height={H}
      />
    {/each}
  {/if}
</div>

{#if (inst.system?.top_processes ?? []).length}
  <SectionHead title="Top processes" />
  <table class="proc-table">
    <thead><tr><th>name</th><th>pid</th><th>cpu</th><th>mem</th></tr></thead>
    <tbody>
      {#each inst.system?.top_processes ?? [] as p (p.pid)}
        <tr>
          <td><code>{p.name}</code></td>
          <td><code>{p.pid}</code></td>
          <td>{p.cpu_percent.toFixed(1)}%</td>
          <td>{p.mem_mb < 1024 ? `${p.mem_mb} MB` : `${(p.mem_mb / 1024).toFixed(1)} GB`}</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<style>
  .section-meta {
    margin-left: var(--space-2);
    opacity: 0.55;
    font-weight: 400;
    font-size: 11px;
    text-transform: none;
    letter-spacing: 0;
  }
  .hist-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    gap: 8px;
    margin: 8px 0 12px;
  }
  .proc-table {
    width: 100%;
    font-size: 12px;
    border-collapse: collapse;
    margin: 6px 0 12px;
  }
  .proc-table th,
  .proc-table td {
    padding: 4px 6px;
    text-align: left;
    border-bottom: 1px solid var(--border-subtle, rgba(255, 255, 255, 0.06));
  }
  .proc-table th {
    font-weight: 500;
    color: var(--text-secondary, rgba(255, 255, 255, 0.6));
  }
  code {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
    font-size: var(--text-xs);
  }
</style>
