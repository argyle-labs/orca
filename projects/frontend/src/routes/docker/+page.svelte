<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { callTool } from '$lib/stores/runTool';
  import type { DockerContainerStats } from '$lib/client/types.gen';

  let containers = $state<DockerContainerStats[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let lastChecked = $state<number | null>(null);
  let pollHandle: ReturnType<typeof setInterval> | null = null;
  const POLL_MS = 5000;

  async function refresh() {
    try {
      const result = await callTool<{ containers: DockerContainerStats[] }>('dockerServiceListStats', {});
      containers = result.containers ?? [];
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
      lastChecked = Date.now();
    }
  }

  onMount(() => {
    refresh();
    pollHandle = setInterval(refresh, POLL_MS);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  function fmtMb(mb: number): string {
    if (mb === 0) return '—';
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    return `${mb} MB`;
  }

  function fmtBytes(b: number): string {
    if (b === 0) return '0 B';
    if (b >= 1073741824) return `${(b / 1073741824).toFixed(1)} GB`;
    if (b >= 1048576) return `${(b / 1048576).toFixed(1)} MB`;
    if (b >= 1024) return `${(b / 1024).toFixed(1)} KB`;
    return `${b} B`;
  }

  function memPct(c: DockerContainerStats): number {
    if (!c.mem_limit_mb || !c.mem_usage_mb) return 0;
    return Math.min(100, (c.mem_usage_mb / c.mem_limit_mb) * 100);
  }

  function relTime(ts: number | null): string {
    if (!ts) return '—';
    const sec = Math.round((Date.now() - ts) / 1000);
    if (sec < 5) return 'just now';
    if (sec < 60) return `${sec}s ago`;
    return `${Math.round(sec / 60)}m ago`;
  }

  const sorted = $derived([...containers].sort((a, b) => b.cpu_percent - a.cpu_percent));
</script>

<section class="page">
  <header>
    <div class="header-row">
      <div>
        <h1>Docker</h1>
        <p class="lede">Live container stats — local host.</p>
      </div>
      <div class="header-actions">
        <span class="checked">checked {relTime(lastChecked)}</span>
        <button class="refresh-btn" onclick={refresh} title="Refresh">↻ Refresh</button>
      </div>
    </div>
  </header>

  {#if loading}
    <p class="hint">Loading…</p>
  {:else if error}
    <div class="err">{error}</div>
  {:else if sorted.length === 0}
    <p class="hint">No running containers found.</p>
  {:else}
    <div class="table-wrap">
      <table>
        <thead>
          <tr>
            <th class="col-name">Container</th>
            <th class="col-cpu">CPU</th>
            <th class="col-ram">RAM</th>
            <th class="col-net">Net I/O</th>
            <th class="col-io">Block I/O</th>
          </tr>
        </thead>
        <tbody>
          {#each sorted as c (c.id)}
            <tr>
              <td class="col-name">
                <span class="name">{c.name.replace(/^\//, '')}</span>
              </td>
              <td class="col-cpu">
                <div class="metric-cell">
                  <div class="bar-wrap">
                    <div
                      class="bar"
                      style="width:{Math.min(100, c.cpu_percent)}%"
                      class:warn={c.cpu_percent > 70}
                      class:crit={c.cpu_percent > 90}
                    ></div>
                  </div>
                  <span class="val">{c.cpu_percent.toFixed(1)}%</span>
                </div>
              </td>
              <td class="col-ram">
                <div class="metric-cell">
                  <div class="bar-wrap">
                    <div
                      class="bar"
                      style="width:{memPct(c)}%"
                      class:warn={memPct(c) > 70}
                      class:crit={memPct(c) > 90}
                    ></div>
                  </div>
                  <span class="val">
                    {fmtMb(c.mem_usage_mb)}
                    {#if c.mem_limit_mb > 0}
                      <span class="dim">/ {fmtMb(c.mem_limit_mb)}</span>
                    {/if}
                  </span>
                </div>
              </td>
              <td class="col-net">
                <span class="io-pair">
                  <span class="io-label">↓</span>{fmtBytes(c.net_rx_bytes)}
                  <span class="sep">/</span>
                  <span class="io-label">↑</span>{fmtBytes(c.net_tx_bytes)}
                </span>
              </td>
              <td class="col-io">
                <span class="io-pair">
                  <span class="io-label">R</span>{fmtBytes(c.block_read_bytes)}
                  <span class="sep">/</span>
                  <span class="io-label">W</span>{fmtBytes(c.block_write_bytes)}
                </span>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <p class="count-hint">{sorted.length} container{sorted.length === 1 ? '' : 's'} · auto-refreshes every 5s</p>
  {/if}
</section>

<style>
  .page {
    max-width: var(--content-max);
    margin: 0 auto;
    padding: var(--space-6) var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
  }

  .header-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-4);
  }

  header h1 { margin: 0 0 var(--space-1); font-size: var(--text-xl); letter-spacing: 0.02em; }
  .lede { margin: 0; color: var(--color-text-muted); font-size: var(--text-sm); }

  .header-actions {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-shrink: 0;
    padding-top: 2px;
  }
  .checked { font-size: var(--text-xs); color: var(--color-text-dim); }
  .refresh-btn {
    height: 26px;
    padding: 0 10px;
    background: var(--color-surface);
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    cursor: pointer;
    font-size: var(--text-xs);
  }
  .refresh-btn:hover { background: var(--color-surface-2); color: var(--color-text); }

  /* ── table ── */
  .table-wrap {
    overflow-x: auto;
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--text-xs);
  }

  thead th {
    background: var(--color-surface);
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: 10px;
    font-weight: var(--weight-semibold);
    padding: 8px 12px;
    text-align: left;
    border-bottom: 1px solid var(--color-border);
    white-space: nowrap;
  }

  tbody tr {
    border-bottom: 1px solid var(--color-border);
  }
  tbody tr:last-child { border-bottom: none; }
  tbody tr:hover { background: var(--color-surface); }

  td {
    padding: 8px 12px;
    vertical-align: middle;
    color: var(--color-text);
  }

  .col-name { min-width: 160px; }
  .col-cpu { min-width: 160px; }
  .col-ram { min-width: 200px; }
  .col-net { min-width: 180px; white-space: nowrap; }
  .col-io { min-width: 180px; white-space: nowrap; }

  .name {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }

  /* ── metric bar cells ── */
  .metric-cell {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .bar-wrap {
    flex: 1;
    height: 5px;
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    overflow: hidden;
  }
  .bar {
    height: 100%;
    background: var(--color-accent, #4f86f7);
    border-radius: 3px;
    transition: width 0.4s ease;
  }
  .bar.warn { background: #e6a817; }
  .bar.crit { background: var(--color-error); }
  .val {
    width: 90px;
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .dim { color: var(--color-text-dim); }

  /* ── I/O pairs ── */
  .io-pair {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    font-variant-numeric: tabular-nums;
  }
  .io-label {
    color: var(--color-text-dim);
    font-size: 10px;
    font-weight: var(--weight-semibold);
    margin-right: 1px;
  }
  .sep { color: var(--color-text-dim); padding: 0 3px; }

  .err { color: var(--color-error); font-size: var(--text-xs); font-family: var(--font-mono); }
  .hint { color: var(--color-text-dim); font-size: var(--text-xs); margin: 0; }
  .count-hint { color: var(--color-text-dim); font-size: var(--text-xs); margin: 0; }
</style>
