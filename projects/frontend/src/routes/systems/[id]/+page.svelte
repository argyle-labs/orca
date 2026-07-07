<script lang="ts">
  import { onMount, onDestroy, untrack } from 'svelte';
  import { page } from '$app/stores';
  import { goto } from '$app/navigation';
  import { callTool } from '$lib/stores/runTool';
  import type {
    TopProcess,
    PodInstance,
    SystemUpdateResponse,
    VersionEntry,
    ChartSeries,
    GpuSeries,
  } from '$lib/client/types.gen';
  import Chart from '$lib/components/primitives/Chart.svelte';
  import type { PageData } from './$types';
  import Modal from '$lib/components/primitives/Modal.svelte';
  import Button from '$lib/components/primitives/Button.svelte';
  import Badge from '$lib/components/primitives/Badge.svelte';
  import SegmentedControl from '$lib/components/primitives/SegmentedControl.svelte';

  let { data }: { data: PageData } = $props();

  let id = $derived($page.params.id);
  // Synchronous seed from load() — peer + probe come from +page.ts so the
  // detail view is fully populated at first paint (no data snap-in when
  // navigating from /). Re-seeded by the $effect below when SvelteKit
  // hands us new `data` for a different `[id]`.
  const seedPeer = untrack(() => {
    const dp = data.peer;
    if (!dp) return null;
    const next: PodInstance = { ...dp };
    const probe = data.probe;
    if (probe) {
      next.version = probe.current_version || next.version;
      next.channel = probe.channel ?? next.channel;
      next.pinned_to = probe.pinned_to ?? next.pinned_to;
      next.update_latest = probe.latest ?? next.update_latest;
      next.update_available = probe.update_available === true;
    }
    return next;
  });
  let peer = $state<PodInstance | null>(seedPeer);
  let loading = $state(false);
  let error = $state<string | null>(
    untrack(() => (data.peer ? null : `peer ${$page.params.id} not found in pod`)),
  );
  let pinnedPid = $state<number | null>(null);
  let updateModalOpen = $state(false);

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
  let channelSelect = $state<'stable' | 'rc'>(
    untrack(() => normalizeChannel(data.probe?.channel ?? seedPeer?.channel)),
  );
  let updatePending = $state(false);
  let channelPending = $state(false);
  let updateResult = $state<{ notes: string[]; errors: string[] } | null>(null);
  let hydratedForId = $state<string | null>(seedPeer?.peer_id ?? null);

  function normalizeChannel(c: string | null | undefined): 'stable' | 'rc' {
    return c === 'rc' ? 'rc' : 'stable';
  }

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
    const next: PodInstance = { ...dp };
    if (data.probe) {
      next.version = data.probe.current_version || next.version;
      next.channel = data.probe.channel ?? next.channel;
      next.pinned_to = data.probe.pinned_to ?? next.pinned_to;
      next.update_latest = data.probe.latest ?? next.update_latest;
      next.update_available = data.probe.update_available === true;
    }
    peer = next;
    error = null;
    versions = (data.probe?.available_versions ?? []) as VersionEntry[];
    versionSelect = data.probe?.current_version
      ? `v${data.probe.current_version}`
      : next.version
        ? `v${next.version}`
        : '';
    channelSelect = normalizeChannel(data.probe?.channel ?? next.channel);
    hydratedForId = next.peer_id;
  });

  let pollHandle: ReturnType<typeof setInterval> | null = null;
  const POLL_MS = 5000;

  async function probeUpdate() {
    if (!peer) return;
    versionsLoading = true;
    try {
      const target = peer.role === 'local' ? null : peer.peer_id;
      const r = await callTool<SystemUpdateResponse>('systemUpdate', {}, { peer: target });
      versions = r.available_versions ?? [];
      if (r.current_version && !versionSelect) versionSelect = `v${r.current_version}`;
      channelSelect = normalizeChannel(r.channel);
      if (peer) {
        peer.version = r.current_version || peer.version;
        peer.channel = r.channel;
        peer.pinned_to = r.pinned_to;
        peer.update_latest = r.latest;
        peer.update_available = r.update_available === true;
      }
    } catch (e) {
      console.warn('probe failed', e);
    } finally {
      versionsLoading = false;
    }
  }

  async function applyUpdate() {
    if (!peer) return;
    if (!versionSelect || versionSelect === `v${peer.version ?? ''}`) return;
    updatePending = true;
    updateResult = null;
    try {
      const target = peer.role === 'local' ? null : peer.peer_id;
      const r = await callTool<SystemUpdateResponse>(
        'systemUpdate',
        { version: versionSelect },
        { peer: target },
      );
      updateResult = { notes: r.notes ?? [], errors: r.errors ?? [] };
      versions = r.available_versions ?? versions;
      if (peer) {
        peer.version = r.current_version || peer.version;
        peer.channel = r.channel;
        peer.pinned_to = r.pinned_to;
        peer.update_latest = r.latest;
        peer.update_available = r.update_available === true;
        if (r.current_version) versionSelect = `v${r.current_version}`;
        channelSelect = normalizeChannel(r.channel);
      }
    } catch (e) {
      updateResult = { notes: [], errors: [e instanceof Error ? e.message : String(e)] };
    } finally {
      updatePending = false;
    }
  }

  // Channel switch is server-side persistence + visibility filter. The Rust
  // side returns available_versions already filtered to the new channel.
  async function changeChannel(next: 'stable' | 'rc') {
    if (!peer || channelPending || next === channelSelect) return;
    channelPending = true;
    channelSelect = next;
    try {
      const target = peer.role === 'local' ? null : peer.peer_id;
      const r = await callTool<SystemUpdateResponse>(
        'systemUpdate',
        { channel: next },
        { peer: target },
      );
      versions = r.available_versions ?? versions;
      if (peer) {
        peer.channel = r.channel;
        peer.pinned_to = r.pinned_to;
        peer.update_latest = r.latest;
        peer.update_available = r.update_available === true;
        if (r.current_version) peer.version = r.current_version;
      }
    } catch (e) {
      console.warn('channel switch failed:', e);
      channelSelect = normalizeChannel(peer?.channel);
    } finally {
      channelPending = false;
    }
  }

  async function refresh() {
    try {
      const r = await callTool<{ members: PodInstance[] }>('podInstances', {});
      const members = r.members ?? [];
      // `id === 'local'` is the synthetic id the list page uses for "this
      // host" — pod.instances flags that row role==='local'.
      const found = id === 'local'
        ? members.find((m) => m.role === 'local')
        : members.find((m) => m.peer_id === id);
      peer = found ?? null;
      error = found ? null : `peer ${id} not found in pod`;
      if (peer && hydratedForId !== peer.peer_id) {
        hydratedForId = peer.peer_id;
        versionSelect = peer.version ? `v${peer.version}` : '';
        channelSelect = normalizeChannel(peer.channel);
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
    void refreshDetailView();
    pollHandle = setInterval(() => {
      void refresh();
      void refreshDetailView();
    }, POLL_MS);
  });
  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  // Re-fetch chart series whenever the visible peer changes (navigating
  // /systems/a → /systems/b reuses this component).
  let lastChartedPeer = $state<string | null>(null);
  $effect(() => {
    if (peer && peer.peer_id !== lastChartedPeer) {
      lastChartedPeer = peer.peer_id;
      void refreshDetailView();
    }
  });

  // ── Charts ──────────────────────────────────────────────────────────────
  // Pre-scaled point series come from `system.detail_view` on the target
  // peer. The daemon owns the segmentation algorithm (timestamp → SVG-space
  // x, value → y, gap detection); the `Chart` primitive is a thin renderer.
  type DetailView = {
    cpu: ChartSeries;
    mem: ChartSeries;
    load: ChartSeries;
    gpus: GpuSeries[];
    samples_count: number;
    window_secs: number;
  };
  const CHART_W = 800;
  const CHART_H = 120;
  let detailView = $state<DetailView | null>(null);
  async function refreshDetailView() {
    if (!peer) return;
    try {
      const target = peer.role === 'local' ? null : peer.peer_id;
      const r = await callTool<DetailView>(
        'systemDetailView',
        { width: CHART_W, height: CHART_H },
        { peer: target },
      );
      detailView = r;
    } catch (e) {
      console.warn('systemDetailView failed', e);
    }
  }

  let procs = $derived<TopProcess[]>(peer?.system?.top_processes ?? []);

  function fmt(n: number | null | undefined, unit: string): string {
    if (n == null || !Number.isFinite(n)) return '—';
    if (unit === '%') return `${n.toFixed(1)}%`;
    if (n < 1024) return `${Math.round(n)} ${unit}`;
    return `${(n / 1024).toFixed(1)} G${unit.slice(1)}`;
  }
</script>

<svelte:head>
  <title>{peer?.label ?? id} — system detail</title>
</svelte:head>

<div class="page">
  <div class="topbar">
    <button class="back" onclick={() => goto('/')}>← Systems</button>
    {#if peer}
      <h1>{peer.label || peer.peer_id}</h1>
      <span class="badge">{peer.system?.system_type ?? 'unknown'}</span>
      {#if peer.version}
        <Button size="sm" onclick={() => (updateModalOpen = true)} title="Open update modal">
          v{peer.version}
          {#if peer.pinned_to}&nbsp;<span title={`Pinned to ${peer.pinned_to}`}>📌</span>{/if}
          {#if peer.update_available && peer.update_latest}
            &nbsp;<Badge color="accent">{peer.update_latest}</Badge>
          {/if}
        </Button>
      {/if}
      {#if peer.channel && peer.channel !== 'dev'}<span class="meta">{peer.channel === 'rc' ? 'preview' : peer.channel}</span>{/if}
    {:else if loading}
      <h1>Loading…</h1>
    {:else}
      <h1>Not found</h1>
    {/if}
  </div>

  {#if error}<div class="errline">{error}</div>{/if}

  {#if peer}
    <Modal open={updateModalOpen} title={`Update ${peer.label || peer.peer_id}`} size="md" onclose={() => (updateModalOpen = false)}>
      <div class="panel-head">
        <Button size="xs" onclick={probeUpdate} disabled={versionsLoading || updatePending} title="Re-probe this peer's update state">
          {versionsLoading ? 'Probing…' : 'Refresh'}
        </Button>
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
        <SegmentedControl
          ariaLabel="Channel"
          value={channelSelect}
          onchange={(v) => void changeChannel(v as 'stable' | 'rc')}
          disabled={updatePending || channelPending}
          items={[
            { label: 'stable', value: 'stable' },
            { label: 'preview', value: 'rc' },
          ]}
        />
      </div>
      {#if !peer.pinned_to && peer.update_available && peer.update_latest}
        <p class="avail">Update available: <code>{peer.update_latest}</code></p>
      {/if}
      <div class="row">
        <span class="row-label"></span>
        <Button
          variant="primary"
          size="sm"
          onclick={applyUpdate}
          disabled={updatePending || !versionSelect || versionSelect === `v${peer.version ?? ''}`}
          title="Install the selected version"
        >{updatePending ? 'Updating…' : 'Apply'}</Button>
      </div>
      {#if updateResult}
        {#if updateResult.notes.length > 0}<p class="ok">{updateResult.notes.join(' · ')}</p>{/if}
        {#if updateResult.errors.length > 0}<p class="errline">{updateResult.errors.join(' · ')}</p>{/if}
      {/if}
    </Modal>
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
      <h2>History <span class="hint">{detailView?.samples_count ?? 0} samples</span></h2>
      {#if !detailView || detailView.samples_count === 0}
        <div class="empty">No history yet — waiting for the first sample.</div>
      {:else}
        <Chart
          label="CPU"
          points={detailView.cpu.points}
          gaps={detailView.cpu.gaps}
          vmax={detailView.cpu.vmax}
          lastValue={detailView.cpu.last_value}
          unit="%"
          color="var(--color-info)"
          width={CHART_W}
          height={CHART_H}
        />
        <Chart
          label="Memory"
          points={detailView.mem.points}
          gaps={detailView.mem.gaps}
          vmax={detailView.mem.vmax}
          lastValue={detailView.mem.last_value}
          unit="%"
          color="var(--color-success)"
          width={CHART_W}
          height={CHART_H}
        />
        {#each detailView.gpus as g}
          <Chart
            label={`GPU: ${g.name}`}
            points={g.utilization.points}
            gaps={g.utilization.gaps}
            vmax={g.utilization.vmax}
            lastValue={g.utilization.last_value}
            unit="%"
            color="var(--color-accent)"
            width={CHART_W}
            height={CHART_H}
          />
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

  .charts h2 { display: flex; align-items: baseline; gap: var(--space-2); }
  .hint { color: var(--color-text-dim); font-size: var(--text-xs); font-weight: var(--weight-normal); }
  .empty {
    color: var(--muted);
    padding: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }

  /* Chart styling lives in the `Chart` primitive now — local chart-cell CSS
     was removed when the snippet was replaced with `<Chart …/>`. */

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
