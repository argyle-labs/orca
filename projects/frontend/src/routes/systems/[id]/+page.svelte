<script lang="ts">
  import { onMount, onDestroy, untrack } from 'svelte';
  import { page } from '$app/stores';
  import { goto } from '$app/navigation';
  import { callTool } from '$lib/stores/runTool';
  import type { SystemInfoReport, SystemHistoryPoint, TopProcess } from '$lib/client/types.gen';
  import type { PageData } from './$types';

  let { data }: { data: PageData } = $props();

  type VersionEntry = { tag: string; prerelease: boolean; published_at: string | null; is_current: boolean };
  type PodPeer = {
    peer_id: string;
    hostname: string;
    addr: string;
    port: number;
    status: string;
    local: boolean;
    version?: string | null;
    channel?: string | null;
    pinned_to?: string | null;
    update_available?: boolean | null;
    update_latest?: string | null;
    system?: SystemInfoReport | null;
  };
  type PodMember = { state: string } & Partial<PodPeer>;
  type SystemUpdateResp = {
    current_version: string;
    channel: string;
    pinned_to: string | null;
    available_versions: VersionEntry[];
    latest: string | null;
    notes?: string[];
    errors?: string[];
  };

  let id = $derived($page.params.id);
  // Synchronous seed from load() — peer + probe come from +page.ts so the
  // detail view is fully populated at first paint (no data snap-in when
  // navigating from /). Re-seeded by the $effect below when SvelteKit
  // hands us new `data` for a different `[id]`.
  const seedPeer = untrack(() => {
    const dp = data.peer;
    if (!dp) return null;
    const next = { ...dp } as unknown as PodPeer;
    const probe = data.probe;
    if (probe) {
      next.version = probe.current_version || next.version;
      next.channel = probe.channel ?? next.channel;
      next.pinned_to = probe.pinned_to ?? next.pinned_to;
      next.update_latest = probe.latest ?? next.update_latest;
      next.update_available =
        !!next.version &&
        !!probe.latest &&
        probe.latest.replace(/^v/, '') !== next.version;
    }
    return next;
  });
  let peer = $state<PodPeer | null>(seedPeer);
  let loading = $state(false);
  let error = $state<string | null>(
    untrack(() => (data.peer ? null : `peer ${$page.params.id} not found in pod`)),
  );
  let pinnedPid = $state<number | null>(null);

  let versions = $state<VersionEntry[]>(
    untrack(() => (data.probe?.available_versions ?? []) as VersionEntry[]),
  );
  let versionsLoading = $state(false);
  let versionSelect = $state(
    untrack(() => {
      const cv = data.probe?.current_version;
      if (cv) return `v${cv}`;
      return seedPeer?.version ? `v${seedPeer.version}` : '';
    }),
  );
  let channelSelect = $state(
    untrack(() =>
      inferChannel(
        data.probe?.current_version ?? seedPeer?.version,
        data.probe?.channel ?? seedPeer?.channel,
      ),
    ),
  );
  let updatePending = $state(false);
  let updateResult = $state<{ notes: string[]; errors: string[] } | null>(null);
  let hydratedForId = $state<string | null>(seedPeer?.peer_id ?? null);

  // Re-seed when SvelteKit reuses the component across `[id]` changes —
  // load() has already produced fresh data.peer/data.probe at that point,
  // so we just copy it into the mutable $state cells. Avoids a snap when
  // navigating /systems/a → /systems/b.
  $effect(() => {
    const dp = data.peer;
    if (!dp) {
      peer = null;
      error = `peer ${id} not found in pod`;
      return;
    }
    if (peer && peer.peer_id === dp.peer_id) return;
    const next = { ...dp } as unknown as PodPeer;
    if (data.probe) {
      next.version = data.probe.current_version || next.version;
      next.channel = data.probe.channel ?? next.channel;
      next.pinned_to = data.probe.pinned_to ?? next.pinned_to;
      next.update_latest = data.probe.latest ?? next.update_latest;
      next.update_available =
        !!next.version &&
        !!data.probe.latest &&
        data.probe.latest.replace(/^v/, '') !== next.version;
    }
    peer = next;
    error = null;
    versions = (data.probe?.available_versions ?? []) as VersionEntry[];
    versionSelect = data.probe?.current_version
      ? `v${data.probe.current_version}`
      : next.version
        ? `v${next.version}`
        : '';
    channelSelect = inferChannel(
      data.probe?.current_version ?? next.version,
      data.probe?.channel ?? next.channel,
    );
    hydratedForId = next.peer_id;
  });

  let pollHandle: ReturnType<typeof setInterval> | null = null;
  const POLL_MS = 5000;

  function inferChannel(v: string | null | undefined, fallback: string | null | undefined): string {
    const s = v ?? '';
    if (/-dev/i.test(s)) return 'dev';
    if (/-rc/i.test(s)) return 'rc';
    if (s) return 'stable';
    return fallback ?? 'stable';
  }

  async function probeUpdate() {
    if (!peer) return;
    versionsLoading = true;
    try {
      const target = peer.local ? null : peer.peer_id;
      const r = await callTool<SystemUpdateResp>('systemUpdate', {}, { peer: target });
      versions = r.available_versions ?? [];
      if (r.current_version && !versionSelect) versionSelect = `v${r.current_version}`;
      channelSelect = inferChannel(r.current_version, r.channel);
      if (peer) {
        peer.version = r.current_version || peer.version;
        peer.channel = r.channel;
        peer.pinned_to = r.pinned_to;
        peer.update_latest = r.latest;
        peer.update_available = !!r.current_version && !!r.latest && r.latest.replace(/^v/, '') !== r.current_version;
      }
    } catch (e) {
      console.warn('probe failed', e);
    } finally {
      versionsLoading = false;
    }
  }

  async function applyUpdate() {
    if (!peer) return;
    const args: Record<string, unknown> = {};
    if (channelSelect && channelSelect !== inferChannel(peer.version, peer.channel)) args.channel = channelSelect;
    if (versionSelect && versionSelect !== `v${peer.version ?? ''}`) args.version = versionSelect;
    if (Object.keys(args).length === 0) return;
    updatePending = true;
    updateResult = null;
    try {
      const target = peer.local ? null : peer.peer_id;
      const r = await callTool<SystemUpdateResp>('systemUpdate', args, { peer: target });
      updateResult = { notes: r.notes ?? [], errors: r.errors ?? [] };
      versions = r.available_versions ?? versions;
      if (peer) {
        peer.version = r.current_version || peer.version;
        peer.channel = r.channel;
        peer.pinned_to = r.pinned_to;
        peer.update_latest = r.latest;
        peer.update_available = !!r.current_version && !!r.latest && r.latest.replace(/^v/, '') !== r.current_version;
        if (r.current_version) versionSelect = `v${r.current_version}`;
        channelSelect = inferChannel(r.current_version, r.channel);
      }
    } catch (e) {
      updateResult = { notes: [], errors: [e instanceof Error ? e.message : String(e)] };
    } finally {
      updatePending = false;
    }
  }

  async function refresh() {
    try {
      const r = await callTool<{ members: PodMember[] }>('podList', {});
      const joined = (r.members ?? [])
        .filter((m) => m.state === 'joined')
        .map((m) => m as unknown as PodPeer);
      // `id === 'local'` is the synthetic id the list page uses for "this
      // host" before pod.list ever assigns a real peer_id. Match the
      // pod.list member flagged `local: true` so the detail page works for
      // the local card too.
      const found = id === 'local'
        ? joined.find((p) => p.local)
        : joined.find((p) => p.peer_id === id);
      peer = found ?? null;
      error = found ? null : `peer ${id} not found in pod`;
      if (peer && hydratedForId !== peer.peer_id) {
        hydratedForId = peer.peer_id;
        versionSelect = peer.version ? `v${peer.version}` : '';
        channelSelect = inferChannel(peer.version, peer.channel);
        void probeUpdate();
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    // Initial peer + probe already populated synchronously from `data`
    // (load() in +page.ts) — onMount only registers the periodic refresh
    // timer. Chose imperative polling over invalidate() so the existing
    // patch-state loop runs unchanged.
    pollHandle = setInterval(refresh, POLL_MS);
  });
  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  // ── Charts ──────────────────────────────────────────────────────────────
  // Series come from server-side `report.history` (ring per peer). NaN
  // breaks the path into segments so dropouts read as gaps, not interpolation.
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
  let gpuNames = $derived(peer?.system?.gpus?.map((g) => g.name) ?? []);
  function gpuSeries(name: string): number[] {
    return history.map((p) => {
      const g = p.gpus?.find((x) => x.name === name);
      return g?.utilization_percent ?? NaN;
    });
  }
  let procs = $derived<TopProcess[]>(peer?.system?.top_processes ?? []);

  // Time labels for the X axis. 3 ticks: oldest, middle, newest — formatted
  // relative to "now" so a 1-hour window reads "-1h / -30m / now".
  function relTime(targetTs: number, nowTs: number): string {
    const dt = nowTs - targetTs;
    if (dt < 5) return 'now';
    if (dt < 60) return `-${Math.round(dt)}s`;
    if (dt < 3600) return `-${Math.round(dt / 60)}m`;
    return `-${(dt / 3600).toFixed(1)}h`;
  }
  let xAxisLabels = $derived.by(() => {
    if (history.length < 2) return [] as string[];
    const now = history[history.length - 1].ts;
    const first = history[0].ts;
    const mid = history[Math.floor(history.length / 2)].ts;
    return [relTime(first, now), relTime(mid, now), 'now'];
  });

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

  {#if error}<div class="errline">{error}</div>{/if}

  {#if peer}
    <section class="panel">
      <div class="panel-head">
        <h2>Update</h2>
        <button class="btn-sm" onclick={probeUpdate} disabled={versionsLoading || updatePending}>
          {versionsLoading ? 'Probing…' : 'Refresh'}
        </button>
      </div>
      <div class="row">
        <span class="row-label">Version{#if peer.pinned_to}<span class="pin" title={`Pinned to ${peer.pinned_to}`}>📌</span>{/if}</span>
        <select bind:value={versionSelect} disabled={updatePending}>
          {#if peer.version && !versions.some((v) => v.tag === `v${peer!.version}`)}
            <option value={`v${peer.version}`}>v{peer.version} (current)</option>
          {/if}
          {#each versions as v}
            <option value={v.tag}>{v.tag}{peer.version && v.tag === `v${peer.version}` ? ' (current)' : ''}</option>
          {/each}
        </select>
      </div>
      <div class="row">
        <span class="row-label">Channel</span>
        <div class="seg">
          {#each ['stable', 'rc', 'dev'] as ch}
            <button class:active={channelSelect === ch} disabled={updatePending} onclick={() => (channelSelect = ch)}>{ch}</button>
          {/each}
        </div>
      </div>
      {#if !peer.pinned_to && peer.update_available && peer.update_latest}
        <p class="avail">Update available: <code>{peer.update_latest}</code></p>
      {/if}
      <div class="row">
        <span class="row-label"></span>
        <button
          class="apply"
          onclick={applyUpdate}
          disabled={updatePending || (!peer.pinned_to && `v${peer.version ?? ''}` === versionSelect && inferChannel(peer.version, peer.channel) === channelSelect)}
        >{updatePending ? 'Updating…' : 'Apply'}</button>
      </div>
      {#if updateResult}
        {#if updateResult.notes.length > 0}<p class="ok">{updateResult.notes.join(' · ')}</p>{/if}
        {#if updateResult.errors.length > 0}<p class="errline">{updateResult.errors.join(' · ')}</p>{/if}
      {/if}
    </section>
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

    {#snippet chartCell(label: string, valStr: string, vals: number[], color: string, vmax: number, unit: string)}
      {@const W = 800}
      {@const H = 120}
      <div class="chart">
        <div class="chart-head">
          <span>{label}</span>
          <span class="chart-val">{valStr}</span>
        </div>
        <div class="chart-body">
          <div class="y-axis">
            <span>{unit === '%' ? '100%' : `${Math.round(vmax)}${unit}`}</span>
            <span>{unit === '%' ? '75%' : `${Math.round(vmax * 0.75)}${unit}`}</span>
            <span>{unit === '%' ? '50%' : `${Math.round(vmax * 0.5)}${unit}`}</span>
            <span>{unit === '%' ? '25%' : `${Math.round(vmax * 0.25)}${unit}`}</span>
            <span>0</span>
          </div>
          <svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" style="color: {color}">
            {#each [0, 0.25, 0.5, 0.75, 1] as g}
              <line x1="0" x2={W} y1={H * (1 - g)} y2={H * (1 - g)}
                stroke="var(--color-border)" stroke-width="0.5"
                opacity={g === 0 || g === 1 ? 0.6 : 0.3} />
            {/each}
            {#each chartSegments(vals, W, H, vmax) as seg}
              <path d={seg.area} fill="currentColor" opacity="0.18" />
              <path d={seg.line} fill="none" stroke="currentColor" stroke-width="1.5" />
            {/each}
          </svg>
        </div>
        <div class="x-axis">
          <span></span>
          {#each xAxisLabels as t}<span>{t}</span>{/each}
        </div>
      </div>
    {/snippet}

    <section class="charts">
      <h2>History <span class="hint">{history.length} samples</span></h2>
      {#if history.length === 0}
        <div class="empty">No history yet — waiting for the first sample.</div>
      {:else}
        {@render chartCell('CPU', fmt(s.cpu_usage_percent, '%'), cpuSeries, 'var(--color-info)', 100, '%')}
        {@render chartCell('Memory', `${fmt(s.mem_used_mb, 'MB')} / ${fmt(s.mem_total_mb, 'MB')}`, memSeries, 'var(--color-success)', 100, '%')}
        {#each gpuNames as name}
          {@render chartCell(`GPU: ${name}`, '', gpuSeries(name), 'var(--color-accent)', 100, '%')}
        {/each}
      {/if}
    </section>

    <section class="panel">
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
  .page {
    max-width: var(--content-max);
    margin: 0 auto;
    padding: var(--space-6);
  }
  .topbar {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin-bottom: var(--space-4);
  }
  .back {
    background: none;
    border: 1px solid var(--border);
    color: var(--text);
    padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--text-sm);
  }
  .back:hover { background: var(--surface); }
  h1 { font-size: var(--text-xl); margin: 0; color: var(--text); }
  h2 { font-size: var(--text-base); margin: 0 0 var(--space-2); color: var(--text); }
  .badge {
    background: var(--code-bg);
    color: var(--muted);
    padding: 2px var(--space-2);
    border-radius: 999px;
    font-size: var(--text-xs);
  }
  .meta { font-size: var(--text-xs); color: var(--muted); }
  .errline { color: var(--color-error); font-size: var(--text-sm); padding: var(--space-1) 0; }
  .ok { color: var(--color-success); font-size: var(--text-sm); margin: var(--space-1) 0; }
  .avail { color: var(--color-warning); font-size: var(--text-sm); margin: var(--space-1) 0; }

  .panel {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: var(--space-4);
    margin-bottom: var(--space-4);
  }
  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: var(--space-2);
  }
  .panel-head h2 { margin: 0; }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: var(--space-3);
    margin-bottom: var(--space-4);
  }
  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: var(--space-3);
  }
  .card-title {
    font-weight: var(--weight-semibold);
    font-size: var(--text-sm);
    margin-bottom: var(--space-2);
    color: var(--text);
  }
  .kv {
    display: flex;
    justify-content: space-between;
    font-size: var(--text-sm);
    padding: 2px 0;
    color: var(--text);
  }
  .kv span { color: var(--muted); }

  .row {
    display: grid;
    grid-template-columns: 110px 1fr;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) 0;
    font-size: var(--text-sm);
  }
  .row-label { color: var(--muted); }
  .row select {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: var(--space-1) var(--space-2);
    font: inherit;
  }
  .pin { margin-left: var(--space-1); }
  .seg {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    overflow: hidden;
  }
  .seg button {
    background: none;
    border: 0;
    color: var(--text);
    padding: var(--space-1) var(--space-3);
    cursor: pointer;
    font-size: var(--text-xs);
  }
  .seg button.active { background: var(--accent); color: var(--color-bg); }
  .btn-sm {
    background: none;
    border: 1px solid var(--border);
    color: var(--text);
    padding: 2px var(--space-2);
    border-radius: var(--radius-sm);
    font-size: var(--text-xs);
    cursor: pointer;
  }
  .btn-sm:hover:not(:disabled) { background: var(--code-bg); }
  .apply {
    background: var(--accent);
    color: var(--color-bg);
    border: 0;
    padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-sm);
    font-size: var(--text-sm);
    cursor: pointer;
  }
  .apply:disabled { opacity: var(--opacity-disabled); cursor: not-allowed; }

  .charts h2 { display: flex; align-items: baseline; gap: var(--space-2); }
  .hint { color: var(--color-text-dim); font-size: var(--text-xs); font-weight: var(--weight-normal); }
  .empty {
    color: var(--muted);
    padding: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }

  /* ── chart ────────────────────────────────────────────────────────────── */
  .chart {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: var(--space-3);
    margin-bottom: var(--space-3);
  }
  .chart-head {
    display: flex;
    justify-content: space-between;
    font-size: var(--text-sm);
    color: var(--text);
    margin-bottom: var(--space-2);
  }
  .chart-val { color: var(--muted); font-variant-numeric: tabular-nums; }
  .chart-body {
    display: grid;
    grid-template-columns: 44px 1fr;
    gap: var(--space-2);
    align-items: stretch;
  }
  .y-axis {
    display: flex;
    flex-direction: column;
    justify-content: space-between;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    text-align: right;
    font-variant-numeric: tabular-nums;
    height: 120px;
  }
  .chart svg {
    width: 100%;
    height: 120px;
    display: block;
  }
  .x-axis {
    display: grid;
    grid-template-columns: 44px repeat(3, 1fr);
    gap: var(--space-2);
    margin-top: var(--space-1);
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    font-variant-numeric: tabular-nums;
  }
  .x-axis > span:nth-child(2) { text-align: left; }
  .x-axis > span:nth-child(3) { text-align: center; }
  .x-axis > span:nth-child(4) { text-align: right; }

  /* ── processes ────────────────────────────────────────────────────────── */
  table { width: 100%; border-collapse: collapse; font-size: var(--text-sm); }
  th, td {
    text-align: left;
    padding: var(--space-1) var(--space-2);
    border-bottom: 1px solid var(--border);
    color: var(--text);
  }
  th { color: var(--muted); font-weight: var(--weight-medium); }
  tbody tr { cursor: pointer; }
  tbody tr:hover { background: var(--code-bg); }
  tbody tr.pinned { background: var(--code-bg); outline: 1px solid var(--accent); outline-offset: -1px; }
</style>
