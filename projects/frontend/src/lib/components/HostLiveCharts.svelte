<script lang="ts">
  import Chart from '$lib/components/primitives/Chart.svelte';
  import SectionHead from '$lib/components/primitives/SectionHead.svelte';
  import type { Instance } from '$lib/types/instance';

  interface Props {
    inst: Instance;
  }
  let { inst }: Props = $props();

  const HIST_LEN = 120;
  type Sample = {
    t: number;
    cpu: number | null;
    memPct: number | null;
    gpuPct: (number | null)[];
  };

  let samples = $state<Sample[]>([]);
  let procMap = $state<Map<number, { name: string; cpu: number[]; mem: number[] }>>(
    new Map(),
  );
  let pinnedPid = $state<number | null>(null);
  let lastId = $state<string | null>(null);

  $effect(() => {
    // Reset on host switch.
    if (inst.id !== lastId) {
      samples = [];
      procMap = new Map();
      pinnedPid = null;
      lastId = inst.id;
    }
    const s = inst.sys;
    if (!s) return;
    const memPct =
      s.mem_total_mb && s.mem_used_mb !== null && s.mem_used_mb !== undefined
        ? (s.mem_used_mb / s.mem_total_mb) * 100
        : null;
    const sample: Sample = {
      t: Date.now(),
      cpu: s.cpu_usage_percent ?? null,
      memPct,
      gpuPct: (s.gpus ?? []).map((g) => g.utilization_percent ?? null),
    };
    samples = [...samples, sample].slice(-HIST_LEN);

    const seen = new Set<number>();
    for (const p of s.top_processes ?? []) {
      seen.add(p.pid);
      const prev = procMap.get(p.pid);
      const next = prev ?? { name: p.name, cpu: [], mem: [] };
      next.cpu = [...next.cpu, p.cpu_percent].slice(-HIST_LEN);
      next.mem = [...next.mem, p.mem_mb].slice(-HIST_LEN);
      next.name = p.name;
      procMap.set(p.pid, next);
    }
    for (const [pid, v] of procMap) {
      if (!seen.has(pid)) {
        v.cpu = [...v.cpu, NaN].slice(-HIST_LEN);
        v.mem = [...v.mem, NaN].slice(-HIST_LEN);
      }
    }
    procMap = new Map(procMap);
  });

  let pinned = $derived(pinnedPid != null ? procMap.get(pinnedPid) : null);
  let pinnedMaxMem = $derived(
    pinned ? Math.max(1, ...pinned.mem.filter(Number.isFinite)) : 1,
  );
</script>

<SectionHead title="Live">
  {#snippet trailing()}
    <span class="section-meta">{samples.length}/{HIST_LEN} samples</span>
  {/snippet}
</SectionHead>
<div class="hist-grid">
  <Chart label="CPU" vals={samples.map((s) => s.cpu ?? NaN)} vmax={100} unit="%" color="#89b4fa" />
  <Chart label="RAM" vals={samples.map((s) => s.memPct ?? NaN)} vmax={100} unit="%" color="#a6e3a1" />
  {#each inst.sys?.gpus ?? [] as g, gi}
    <Chart
      label={g.name || `GPU ${gi}`}
      vals={samples.map((s) => s.gpuPct?.[gi] ?? NaN)}
      vmax={100}
      unit="%"
      color="#f5c2e7"
    />
  {/each}
</div>

{#if (inst.sys?.top_processes ?? []).length}
  <SectionHead title="Top processes">
    {#snippet trailing()}
      <span class="section-meta">click to pin</span>
    {/snippet}
  </SectionHead>
  <table class="proc-table">
    <thead><tr><th>name</th><th>pid</th><th>cpu</th><th>mem</th></tr></thead>
    <tbody>
      {#each inst.sys?.top_processes ?? [] as p (p.pid)}
        <tr
          class:pinned={pinnedPid === p.pid}
          onclick={() => {
            pinnedPid = pinnedPid === p.pid ? null : p.pid;
          }}
        >
          <td><code>{p.name}</code></td>
          <td><code>{p.pid}</code></td>
          <td>{p.cpu_percent.toFixed(1)}%</td>
          <td>{p.mem_mb < 1024 ? `${p.mem_mb} MB` : `${(p.mem_mb / 1024).toFixed(1)} GB`}</td>
        </tr>
      {/each}
    </tbody>
  </table>
  {#if pinned}
    <div class="hist-grid">
      <Chart label={`${pinned.name} CPU`} vals={pinned.cpu} vmax={100} unit="%" color="#fab387" />
      <Chart label={`${pinned.name} RAM`} vals={pinned.mem} vmax={pinnedMaxMem} unit="MB" color="#cba6f7" />
    </div>
  {/if}
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
  .proc-table tbody tr {
    cursor: pointer;
  }
  .proc-table tbody tr:hover {
    background: var(--bg-elevated, rgba(255, 255, 255, 0.04));
  }
  .proc-table tr.pinned {
    background: rgba(137, 180, 250, 0.15);
  }
  code {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
    font-size: var(--text-xs);
  }
</style>
