<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { page } from '$app/stores';
  import { goto } from '$app/navigation';
  import { callTool } from '$lib/stores/runTool';
  import type { SystemInfoReport, SystemHistoryPoint, TopProcess } from '$lib/client/types.gen';

  type PodPeer = {
    peer_id: string;
    hostname: string;
    addr: string;
    port: number;
    status: string;
    local: boolean;
    version?: string | null;
    channel?: string | null;
    system?: SystemInfoReport | null;
  };
  type PodMember = { state: string } & Partial<PodPeer>;

  let id = $derived($page.params.id);
  let peer = $state<PodPeer | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let pinnedPid = $state<number | null>(null);

  let pollHandle: ReturnType<typeof setInterval> | null = null;
  const POLL_MS = 5000;

  async function refresh() {
    try {
      const r = await callTool<{ members: PodMember[] }>('podList', {});
      const found = (r.members ?? [])
        .filter((m) => m.state === 'joined')
        .map((m) => m as unknown as PodPeer)
        .find((p) => p.peer_id === id);
      peer = found ?? null;
      error = found ? null : `peer ${id} not found in pod`;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    void refresh();
    pollHandle = setInterval(refresh, POLL_MS);
  });
  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  // ── Chart rendering ────────────────────────────────────────────────────
  // History points come from the daemon (`report.history`) — server-side
  // ring, survives drawer/page navigation, capped at ~720 points (≈1 h).
  function chartSegments(
    vals: number[],
    W: number,
    H: number,
    vmax: number,
  ): { line: string; area: string }[] {
    const out: { line: string; area: string }[] = [];
    if (!vals.length || vmax <= 0) return out;
    const n = vals.length;
    let line = '';
    let area = '';
    let segStartX: number | null = null;
    let segLastX: number | null = null;
    const flush = () => {
      if (line) {
        out.push({
          line: line.trim(),
          area: `${area} L ${segLastX!.toFixed(1)} ${H} L ${segStartX!.toFixed(1)} ${H} Z`.trim(),
        });
      }
      line = '';
      area = '';
      segStartX = null;
      segLastX = null;
    };
    for (let i = 0; i < n; i++) {
      const v = vals[i];
      const x = (i / Math.max(1, n - 1)) * W;
      if (!Number.isFinite(v)) {
        flush();
        continue;
      }
      const y = H - (Math.min(Math.max(v, 0), vmax) / vmax) * H;
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
  }

  let history = $derived<SystemHistoryPoint[]>(peer?.system?.history ?? []);

  let cpuSeries = $derived(history.map((p) => p.cpu_percent ?? NaN));
  let memSeries = $derived(
    history.map((p) =>
      p.mem_used_mb != null && p.mem_total_mb && p.mem_total_mb > 0
        ? (p.mem_used_mb / p.mem_total_mb) * 100
        : NaN,
    ),
  );

  // GPUs: group history points by GPU name. Order matches current report.gpus.
  let gpuNames = $derived(peer?.system?.gpus?.map((g) => g.name) ?? []);
  function gpuSeries(name: string): number[] {
    return history.map((p) => {
      const g = p.gpus?.find((x) => x.name === name);
      return g?.utilization_percent ?? NaN;
    });
  }

  let procs = $derived<TopProcess[]>(peer?.system?.top_processes ?? []);
  // Per-process series are NOT in server history (would explode size).
  // The pinned-process chart shows last value only until we add server-side
  // per-process retention — for now collapse to a single bar.

  function fmt(n: number | null | undefined, unit: string): string {
    if (n == null || !Number.isFinite(n)) return '—';
    if (unit === '%') return `${n.toFixed(1)}%`;
    if (n < 1024) return `${Math.round(n)} ${unit}`;
    return `${(n / 1024).toFixed(1)} G${unit.slice(1)}`;
  }
</script>

<svelte:head>
  <title>{peer?.hostname ?? id} — system detail</title>
</svelte:head>

<div class="page">
  <div class="topbar">
    <button class="back" onclick={() => goto('/')}>← Systems</button>
    {#if peer}
      <h1>{peer.hostname || peer.peer_id}</h1>
      <span class="badge">{peer.system?.system_type ?? 'unknown'}</span>
      {#if peer.version}<span class="meta">v{peer.version}</span>{/if}
      {#if peer.channel}<span class="meta">{peer.channel}</span>{/if}
    {:else if loading}
      <h1>Loading…</h1>
    {:else}
      <h1>Not found</h1>
    {/if}
  </div>

  {#if error}
    <div class="err">{error}</div>
  {/if}

  {#if peer?.system}
    {@const s = peer.system}
    <section class="grid">
      <div class="card">
        <div class="card-title">CPU</div>
        <div class="kv"><span>Model</span><b>{s.cpu_model ?? '—'}</b></div>
        <div class="kv"><span>Cores</span><b>{s.cpu_physical ?? '?'}p / {s.cpu_logical ?? '?'}l</b></div>
        <div class="kv"><span>Load 1m</span><b>{s.load_avg_1?.toFixed(2) ?? '—'}</b></div>
      </div>
      <div class="card">
        <div class="card-title">Memory</div>
        <div class="kv"><span>Total</span><b>{fmt(s.mem_total_mb, 'MB')}</b></div>
        <div class="kv"><span>Used</span><b>{fmt(s.mem_used_mb, 'MB')}</b></div>
        <div class="kv"><span>Swap</span><b>{fmt(s.swap_used_mb, 'MB')} / {fmt(s.swap_total_mb, 'MB')}</b></div>
      </div>
      <div class="card">
        <div class="card-title">Host</div>
        <div class="kv"><span>OS</span><b>{s.distro ?? s.os_name ?? '—'}</b></div>
        <div class="kv"><span>Kernel</span><b>{s.kernel_version ?? '—'}</b></div>
        <div class="kv"><span>Uptime</span><b>{s.system_uptime_secs ? `${Math.floor(s.system_uptime_secs / 3600)}h` : '—'}</b></div>
      </div>
    </section>

    <section class="charts">
      <h2>History <span class="hint">{history.length} samples</span></h2>
      {#if history.length === 0}
        <div class="empty">No history yet — waiting for the first sample.</div>
      {:else}
        {@const W = 800}
        {@const H = 120}
        <div class="chart">
          <div class="chart-label">CPU<span class="chart-val">{fmt(s.cpu_usage_percent, '%')}</span></div>
          <svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" style="color:#89b4fa">
            {#each [0.25, 0.5, 0.75] as g}
              <line x1="0" x2={W} y1={H * (1 - g)} y2={H * (1 - g)} stroke="currentColor" stroke-width="0.5" opacity="0.15" />
            {/each}
            {#each chartSegments(cpuSeries, W, H, 100) as seg}
              <path d={seg.area} fill="currentColor" opacity="0.18" />
              <path d={seg.line} fill="none" stroke="currentColor" stroke-width="1.5" />
            {/each}
          </svg>
          <div class="chart-axis"><span>0</span><span>100%</span></div>
        </div>

        <div class="chart">
          <div class="chart-label">Memory<span class="chart-val">{fmt(s.mem_used_mb, 'MB')} / {fmt(s.mem_total_mb, 'MB')}</span></div>
          <svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" style="color:#a6e3a1">
            {#each [0.25, 0.5, 0.75] as g}
              <line x1="0" x2={W} y1={H * (1 - g)} y2={H * (1 - g)} stroke="currentColor" stroke-width="0.5" opacity="0.15" />
            {/each}
            {#each chartSegments(memSeries, W, H, 100) as seg}
              <path d={seg.area} fill="currentColor" opacity="0.18" />
              <path d={seg.line} fill="none" stroke="currentColor" stroke-width="1.5" />
            {/each}
          </svg>
          <div class="chart-axis"><span>0</span><span>100%</span></div>
        </div>

        {#each gpuNames as name}
          <div class="chart">
            <div class="chart-label">GPU: {name}</div>
            <svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" style="color:#f5c2e7">
              {#each [0.25, 0.5, 0.75] as g}
                <line x1="0" x2={W} y1={H * (1 - g)} y2={H * (1 - g)} stroke="currentColor" stroke-width="0.5" opacity="0.15" />
              {/each}
              {#each chartSegments(gpuSeries(name), W, H, 100) as seg}
                <path d={seg.area} fill="currentColor" opacity="0.18" />
                <path d={seg.line} fill="none" stroke="currentColor" stroke-width="1.5" />
              {/each}
            </svg>
            <div class="chart-axis"><span>0</span><span>100%</span></div>
          </div>
        {/each}
      {/if}
    </section>

    <section class="procs">
      <h2>Top processes</h2>
      <table>
        <thead>
          <tr><th>PID</th><th>Name</th><th>CPU%</th><th>RSS</th></tr>
        </thead>
        <tbody>
          {#each procs as p}
            <tr
              class:pinned={pinnedPid === p.pid}
              onclick={() => (pinnedPid = pinnedPid === p.pid ? null : p.pid)}
            >
              <td>{p.pid}</td>
              <td>{p.name}</td>
              <td>{p.cpu_percent.toFixed(1)}</td>
              <td>{fmt(p.mem_mb, 'MB')}</td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if pinnedPid != null}
        <div class="hint">Per-process history is not yet retained server-side; pinning shows current values only.</div>
      {/if}
    </section>
  {/if}
</div>

<style>
  .page { padding: 1rem 1.5rem; max-width: 1100px; margin: 0 auto; }
  .topbar { display: flex; align-items: center; gap: 0.75rem; margin-bottom: 1rem; }
  .back { background: none; border: 1px solid #444; color: inherit; padding: 0.25rem 0.6rem; border-radius: 4px; cursor: pointer; }
  h1 { font-size: 1.4rem; margin: 0; }
  .badge { background: #313244; padding: 0.1rem 0.5rem; border-radius: 999px; font-size: 0.75rem; }
  .meta { font-size: 0.8rem; opacity: 0.7; }
  .err { color: #f38ba8; padding: 0.5rem 0; }
  .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 0.75rem; margin-bottom: 1.25rem; }
  .card { background: #1e1e2e; border: 1px solid #313244; border-radius: 6px; padding: 0.75rem; }
  .card-title { font-weight: 600; font-size: 0.85rem; margin-bottom: 0.4rem; opacity: 0.85; }
  .kv { display: flex; justify-content: space-between; font-size: 0.85rem; padding: 0.1rem 0; }
  .kv span { opacity: 0.7; }
  .charts h2, .procs h2 { font-size: 0.95rem; margin: 1rem 0 0.5rem; }
  .hint { opacity: 0.6; font-size: 0.75rem; font-weight: normal; }
  .empty { opacity: 0.6; padding: 1rem; background: #1e1e2e; border-radius: 6px; }
  .chart { background: #1e1e2e; border: 1px solid #313244; border-radius: 6px; padding: 0.5rem 0.75rem; margin-bottom: 0.75rem; }
  .chart-label { display: flex; justify-content: space-between; font-size: 0.8rem; margin-bottom: 0.25rem; }
  .chart-val { opacity: 0.7; font-variant-numeric: tabular-nums; }
  .chart svg { width: 100%; height: 120px; display: block; }
  .chart-axis { display: flex; justify-content: space-between; font-size: 0.7rem; opacity: 0.5; padding-top: 0.15rem; }
  .procs table { width: 100%; border-collapse: collapse; font-size: 0.85rem; }
  .procs th, .procs td { text-align: left; padding: 0.25rem 0.5rem; border-bottom: 1px solid #313244; }
  .procs tbody tr { cursor: pointer; }
  .procs tbody tr:hover { background: #2a2a3a; }
  .procs tbody tr.pinned { background: #383850; }
</style>
