<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { callTool } from '$lib/stores/runTool';
  import StatusDot from '$lib/components/StatusDot.svelte';
  import type { GpuInfo, SystemInfoReport } from '$lib/client/types.gen';

  interface Instance {
    id: string;
    peerId: string;
    label: string;
    origin: string;
    port: number;
    role: 'local' | 'system';
    version: string | null;
    target: string | null;
    mode: string | null;
    channel: string | null;
    health: 'up' | 'down' | 'unknown';
    error: string | null;
    lastChecked: number | null;
    secure?: { local: boolean; peer: boolean } | null;
    status?: string | null;
    addresses?: { kind: string; value: string }[] | null;
    sys?: SystemInfoReport | null;
  }

  let instances = $state<Instance[]>([]);
  let selectedInstId = $state<string | null>(null);
  let trustPending = $state(false);
  let retentionDays = $state(1);
  let pollHandle: ReturnType<typeof setInterval> | null = null;

  // Drawer update controls — reset only when the SELECTED INSTANCE changes,
  // not on every poll tick that updates instance data.
  let drawerChannel = $state('stable');
  let drawerOpenedForId = $state<string | null>(null);
  let updateCheckResult = $state<{
    channel: string;
    latest: string | null;
    up_to_date: boolean;
  } | null>(null);
  let updateResult = $state<{ done: string[]; errors: string[] } | null>(null);
  let checkPending = $state(false);
  let updatePending = $state(false);

  // 1-second live poll; DB writes happen every 10 s (host_status_writer)
  const POLL_MS = 1000;

  const RETENTION_OPTIONS = [
    { label: '1 day', value: 1 },
    { label: '7 days', value: 7 },
    { label: '30 days', value: 30 },
  ];

  let selectedInst = $derived(instances.find((i) => i.id === selectedInstId) ?? null);

  function originForLocal(): string {
    if (typeof window === 'undefined') return '';
    return window.location.origin;
  }

  // Pick the most useful display URL for a card — prefer FQDN.
  function primaryUrl(inst: Instance): string {
    if (inst.role === 'local') return inst.origin;
    const addrs = inst.addresses ?? [];
    const fqdnAddr = addrs.find((a) => a.kind === 'fqdn');
    if (fqdnAddr) return fqdnAddr.value;
    if (inst.sys?.fqdn) return `${inst.sys.fqdn}:${inst.port}`;
    const isIp = /^\d+\.\d+\.\d+\.\d+$|^[0-9a-f:]+$/i.test(inst.label);
    if (!isIp) return `${inst.label}:${inst.port}`;
    const lan = addrs.find((a) => a.kind === 'lan_v4');
    if (lan) return `${lan.value}:${inst.port}`;
    return inst.origin;
  }

  async function refreshLocal(inst: Instance) {
    try {
      const [health, spec] = await Promise.all([
        callTool('ping', {}),
        callTool('systemRuntimeDetail', {}),
      ]);
      inst.health = (health as { ok: boolean }).ok ? 'up' : 'down';
      const s = spec as {
        version: string;
        target: string;
        frontend: string;
        mode?: string;
        channel?: string;
        system?: SystemInfoReport | null;
      };
      inst.version = s.version ?? null;
      inst.target = s.target ?? null;
      inst.mode = s.mode ?? null;
      inst.channel = s.channel ?? null;
      inst.sys = s.system ?? null;
      inst.error = null;
    } catch (e) {
      inst.health = 'down';
      inst.error = e instanceof Error ? e.message : String(e);
    } finally {
      inst.lastChecked = Date.now();
      instances = [...instances];
    }
  }

  async function refreshPodPeers() {
    try {
      const [peersResult, statusResult] = await Promise.all([
        callTool<{
          peer_id: string;
          hostname: string;
          addr: string;
          port: number;
          status: string;
          local_secure: boolean;
          peer_secure: boolean;
          local: boolean;
          version?: string | null;
          mode?: string | null;
          channel?: string | null;
          addresses?: { kind: string; value: string }[];
          system?: SystemInfoReport | null;
        }[]>('podPeerList', {}),
        callTool<{ peer_id: string; system?: SystemInfoReport | null }[]>(
          'hostStatusList',
          {},
        ).catch(() => []),
      ]);

      const sysById = new Map<string, SystemInfoReport | null>();
      for (const row of statusResult ?? []) {
        sysById.set(row.peer_id, row.system ?? null);
      }

      const local = instances.find((i) => i.role === 'local');
      // Filter out the synthetic local-host row — it duplicates the LOCAL card.
      const podRows: Instance[] = (peersResult ?? [])
        .filter((p) => !p.local)
        .map((p) => {
          const storedSys = sysById.get(p.peer_id) ?? null;
          const sys = p.system ?? storedSys;
          return {
            id: `system:${p.peer_id}`,
            peerId: p.peer_id,
            label: p.hostname || p.peer_id,
            origin: `${p.addr}:${p.port}`,
            port: p.port,
            role: 'system' as const,
            // Probe version first; fall back to the version field in the stored snapshot.
            version: p.version ?? sys?.version ?? null,
            target: p.target ?? null,
            mode: p.mode ?? null,
            channel: p.channel ?? null,
            health: p.status === 'active' ? 'up' : 'down',
            error: null,
            lastChecked: Date.now(),
            secure: { local: p.local_secure, peer: p.peer_secure },
            status: p.status,
            addresses: (p.addresses ?? []).map((a) => ({ kind: a.kind, value: a.value })),
            sys,
          };
        });
      instances = local ? [local, ...podRows] : podRows;
    } catch (e) {
      console.warn('pod.peer.list failed:', e);
    }
  }

  function refresh(inst: Instance, e?: MouseEvent) {
    e?.stopPropagation();
    if (inst.role === 'local') return refreshLocal(inst);
    return refreshPodPeers();
  }

  async function toggleLocalTrust(inst: Instance) {
    if (!inst.secure || trustPending) return;
    trustPending = true;
    try {
      await callTool('podPeerUpdate', { peer_id: inst.peerId, on: !inst.secure.local });
      await refreshPodPeers();
    } catch (e) {
      console.warn('trust toggle failed:', e);
    } finally {
      trustPending = false;
    }
  }

  async function loadRetention() {
    try {
      const data = await callTool<{ row: { json: string } | null }>('configGet', {
        noun: 'host_status',
        name: 'retention_days',
      });
      if (data?.row) retentionDays = parseFloat(data.row.json) || 1;
    } catch {
      // default 1 day
    }
  }

  async function setRetention(days: number) {
    retentionDays = days;
    try {
      await callTool('configSet', {
        noun: 'host_status',
        name: 'retention_days',
        json: String(days),
      });
    } catch (e) {
      console.warn('retention set failed:', e);
    }
  }

  function closeDrawer() {
    selectedInstId = null;
  }

  $effect(() => {
    // Only reset controls when opening a DIFFERENT instance's drawer.
    // Polling updates selectedInst data without changing its id — don't
    // clobber the user's channel selection on every tick.
    if (selectedInst && selectedInst.id !== drawerOpenedForId) {
      drawerOpenedForId = selectedInst.id;
      drawerChannel = selectedInst.channel ?? 'stable';
      updateCheckResult = null;
      updateResult = null;
    }
  });

  async function checkUpdate() {
    if (!selectedInst) return;
    checkPending = true;
    updateCheckResult = null;
    try {
      const args: Record<string, unknown> = { channel: drawerChannel };
      if (selectedInst.role === 'system') args.peer_id = selectedInst.peerId;
      const r = await callTool<{
        channel: string;
        latest?: string | null;
        up_to_date: boolean;
      }>('systemUpdateDetail', args);
      updateCheckResult = {
        channel: r.channel,
        latest: r.latest ?? null,
        up_to_date: r.up_to_date,
      };
    } catch (e) {
      console.warn('update check failed:', e);
    } finally {
      checkPending = false;
    }
  }

  async function applyUpdate() {
    if (!selectedInst) return;
    updatePending = true;
    updateResult = null;
    try {
      const args: Record<string, unknown> = { channel: drawerChannel };
      if (selectedInst.role === 'system') args.peer_id = selectedInst.peerId;
      const r = await callTool<{ done: string[]; skipped: string[]; errors: string[] }>(
        'systemUpdateCreate',
        args,
      );
      updateResult = { done: r.done, errors: r.errors };
    } catch (e) {
      console.warn('update failed:', e);
      updateResult = { done: [], errors: [e instanceof Error ? e.message : String(e)] };
    } finally {
      updatePending = false;
    }
  }

  onMount(() => {
    const local: Instance = {
      id: 'local',
      peerId: 'local',
      label: 'local',
      origin: originForLocal(),
      port: 12000,
      role: 'local',
      version: null,
      target: null,
      mode: null,
      channel: null,
      health: 'unknown',
      error: null,
      lastChecked: null,
      sys: null,
    };
    instances = [local];
    refreshLocal(local);
    refreshPodPeers();
    loadRetention();
    pollHandle = setInterval(() => {
      const loc = instances.find((i) => i.role === 'local');
      if (loc) refreshLocal(loc);
      refreshPodPeers();
    }, POLL_MS);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  function relTime(ts: number | null): string {
    if (!ts) return '—';
    const sec = Math.round((Date.now() - ts) / 1000);
    if (sec < 5) return 'just now';
    if (sec < 60) return `${sec}s ago`;
    if (sec < 3600) return `${Math.round(sec / 60)}m ago`;
    return `${Math.round(sec / 3600)}h ago`;
  }

  function fmtMb(mb: number | null | undefined): string {
    if (mb == null) return '—';
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    return `${mb} MB`;
  }

  function memPct(sys: SystemInfoReport | null | undefined): number {
    if (!sys?.mem_total_mb || !sys?.mem_used_mb) return 0;
    return Math.min(100, (sys.mem_used_mb / sys.mem_total_mb) * 100);
  }

  function cpuPct(sys: SystemInfoReport | null | undefined): number | null {
    return sys?.cpu_usage_percent ?? null;
  }

  function fmtGpu(g: GpuInfo): string {
    const util = g.utilization_percent != null ? ` ${g.utilization_percent.toFixed(0)}%` : '';
    return `${g.name}${util}`;
  }
</script>

<section class="page">
  <header>
    <div class="title-row">
      <h1>Systems</h1>
      <div class="retention-picker">
        <span class="retention-label">History</span>
        <select
          value={retentionDays}
          onchange={(e) => setRetention(Number((e.target as HTMLSelectElement).value))}
        >
          {#each RETENTION_OPTIONS as opt}
            <option value={opt.value} selected={opt.value === retentionDays}>{opt.label}</option>
          {/each}
        </select>
      </div>
    </div>
    <p class="lede">Connected orca instances.</p>
  </header>

  <div class="instances">
    {#each instances as inst (inst.id)}
      <div
        class="instance"
        class:down={inst.health === 'down'}
        onclick={() => (selectedInstId = inst.id)}
        role="button"
        tabindex="0"
        onkeydown={(e) => e.key === 'Enter' && (selectedInstId = inst.id)}
      >
        <div class="card-header">
          <div class="ident">
            <StatusDot ok={inst.health === 'up' ? true : inst.health === 'down' ? false : null} />
            <span class="hostname">{inst.sys?.hostname ?? inst.label}</span>
          </div>
          <button class="icon-btn" onclick={(e) => refresh(inst, e)} title="Refresh">↻</button>
        </div>

        {#if inst.sys}
          <div class="metrics">
            <div class="metric-row">
              <span class="metric-label">CPU</span>
              <div class="bar-wrap">
                <div
                  class="bar"
                  style="width:{cpuPct(inst.sys) ?? 0}%"
                  class:warn={(cpuPct(inst.sys) ?? 0) > 70}
                  class:crit={(cpuPct(inst.sys) ?? 0) > 90}
                ></div>
              </div>
              <span class="metric-val">
                {cpuPct(inst.sys) != null ? `${cpuPct(inst.sys)!.toFixed(1)}%` : '—'}
              </span>
            </div>
            <div class="metric-row">
              <span class="metric-label">RAM</span>
              <div class="bar-wrap">
                <div
                  class="bar"
                  style="width:{memPct(inst.sys)}%"
                  class:warn={memPct(inst.sys) > 70}
                  class:crit={memPct(inst.sys) > 90}
                ></div>
              </div>
              <span class="metric-val">
                {memPct(inst.sys).toFixed(1)}% <span class="dim">{fmtMb(inst.sys.mem_used_mb)} / {fmtMb(inst.sys.mem_total_mb)}</span>
              </span>
            </div>
            {#if inst.sys.load_avg_1 != null}
              <div class="metric-row load-row">
                <span class="metric-label">Load</span>
                <span class="load-val">
                  {inst.sys.load_avg_1.toFixed(2)}<span class="dim">
                    &nbsp;/ {inst.sys.load_avg_5?.toFixed(2) ?? '—'} / {inst.sys.load_avg_15?.toFixed(2) ?? '—'}&nbsp;<span class="load-legend">1m/5m/15m</span>
                  </span>
                </span>
              </div>
            {/if}
            {#if inst.sys.gpus?.length}
              {#each inst.sys.gpus as g}
                <div class="metric-row">
                  <span class="metric-label">GPU</span>
                  <div class="bar-wrap">
                    <div
                      class="bar"
                      style="width:{g.utilization_percent ?? 0}%"
                      class:warn={(g.utilization_percent ?? 0) > 70}
                      class:crit={(g.utilization_percent ?? 0) > 90}
                    ></div>
                  </div>
                  <span class="metric-val gpu-val">
                    {g.utilization_percent != null ? `${g.utilization_percent.toFixed(0)}%` : '—'} <span class="dim gpu-name">{g.name}</span>
                  </span>
                </div>
              {/each}
            {/if}
          </div>
        {/if}

        <div class="card-footer">
          <span class="primary-url">{primaryUrl(inst)}</span>
          <span class="details-hint">Details →</span>
        </div>
      </div>
    {/each}
  </div>

  {#if instances.filter((i) => i.role === 'system').length === 0}
    <p class="hint">
      No paired systems yet. Run <code>orca pod init</code> to become a founder,
      or <code>orca pod accept &lt;code&gt;</code> on a joiner to pair with an existing pod.
    </p>
  {/if}
</section>

<!-- Drawer -->
{#if selectedInst}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div class="backdrop" role="presentation" onclick={closeDrawer}></div>
  <aside class="drawer">
    <div class="drawer-header">
      <div class="ident">
        <StatusDot
          ok={selectedInst.health === 'up'
            ? true
            : selectedInst.health === 'down'
              ? false
              : null}
        />
        <span class="hostname">{selectedInst.sys?.hostname ?? selectedInst.label}</span>
      </div>
      <button class="icon-btn" onclick={closeDrawer} title="Close">✕</button>
    </div>

    <div class="drawer-body">
      <dl class="detail-grid">
        <dt>Origin</dt>
        <dd><code>{selectedInst.origin}</code></dd>

        {#if selectedInst.status}
          <dt>Status</dt>
          <dd><code>{selectedInst.status}</code></dd>
        {/if}

        {#if selectedInst.sys?.os_name}
          <dt>OS</dt>
          <dd>
            <code
              >{selectedInst.sys.os_name}{selectedInst.sys.os_version
                ? ` ${selectedInst.sys.os_version}`
                : ''}</code
            >
          </dd>
        {/if}

        {#if selectedInst.version}
          <dt>Version</dt>
          <dd><code>{selectedInst.version}</code></dd>
        {/if}

        {#if selectedInst.target}
          <dt>Target</dt>
          <dd><code>{selectedInst.target}</code></dd>
        {/if}

        {#if selectedInst.mode}
          <dt>Mode</dt>
          <dd><code>{selectedInst.mode}</code></dd>
        {/if}

        {#if selectedInst.channel}
          <dt>Channel</dt>
          <dd><code>{selectedInst.channel}</code></dd>
        {/if}

        {#if selectedInst.sys?.virtualization && selectedInst.sys.virtualization !== 'none'}
          <dt>Virt</dt>
          <dd><code>{selectedInst.sys.virtualization}</code></dd>
        {/if}

        {#if selectedInst.sys?.gpus?.length}
          <dt>GPU</dt>
          <dd>
            {#each selectedInst.sys.gpus as g}
              <code>{fmtGpu(g)}</code>
            {/each}
          </dd>
        {/if}

        <dt>Checked</dt>
        <dd>{relTime(selectedInst.lastChecked)}</dd>
      </dl>

      {#if (selectedInst.addresses ?? []).length > 0}
        <div class="section-head">Addresses</div>
        <dl class="addr-grid">
          {#each selectedInst.addresses ?? [] as a (a.kind + ':' + a.value)}
            <dt>{a.kind}</dt>
            <dd><code>{a.value}</code></dd>
          {/each}
        </dl>
      {/if}

      {#if selectedInst.role === 'system' && selectedInst.secure}
        <div class="section-head">Trust</div>
        <div class="trust-row">
          <span class="trust-label">Local</span>
          <div class="trust-toggles">
            <button
              class="trust-btn"
              class:active={selectedInst.secure.local}
              disabled={trustPending}
              onclick={() => toggleLocalTrust(selectedInst!)}
            >
              {selectedInst.secure.local ? 'Trusted' : 'Not trusted'}
            </button>
          </div>
        </div>
        <div class="trust-row">
          <span class="trust-label">Peer</span>
          <span class="trust-readonly">
            {selectedInst.secure.peer ? 'trusts us' : 'does not trust us'}
          </span>
        </div>
      {/if}

      <div class="section-head">Update</div>
      <div class="update-controls">
        <div class="update-row">
          <select bind:value={drawerChannel} class="channel-select">
            <option value="stable">stable</option>
            <option value="rc">rc</option>
            <option value="beta">beta</option>
          </select>
          <button class="ctrl-btn" onclick={checkUpdate} disabled={checkPending || updatePending}>
            {checkPending ? '…' : 'Check'}
          </button>
          <button
            class="ctrl-btn primary"
            onclick={applyUpdate}
            disabled={updatePending || checkPending}
          >
            {updatePending ? 'Updating…' : 'Update'}
          </button>
        </div>

        {#if updateCheckResult}
          <p class="update-status" class:ok={updateCheckResult.up_to_date}>
            {updateCheckResult.up_to_date
              ? `Up to date${updateCheckResult.latest ? ` (${updateCheckResult.latest})` : ''}`
              : `Available: ${updateCheckResult.latest ?? '?'}`}
          </p>
        {/if}

        {#if updateResult}
          {#if updateResult.done.length > 0}
            <p class="update-status ok">{updateResult.done.join(' · ')}</p>
          {/if}
          {#if updateResult.errors.length > 0}
            <p class="err">{updateResult.errors.join(' · ')}</p>
          {/if}
        {/if}
      </div>

      {#if selectedInst.error}
        <div class="err">{selectedInst.error}</div>
      {/if}
    </div>
  </aside>
{/if}

<style>
  .page {
    max-width: var(--content-max);
    margin: 0 auto;
    padding: var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
  }

  .title-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }

  header h1 {
    margin: 0 0 var(--space-1);
    font-size: var(--text-xl);
    letter-spacing: 0.02em;
  }
  .lede {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .retention-picker {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-shrink: 0;
  }
  .retention-label {
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .retention-picker select {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm, 4px);
    color: var(--color-text);
    font-size: var(--text-xs);
    padding: 2px 6px;
    cursor: pointer;
  }

  /* ── grid ─────────────────────────────────────────────────────────────── */
  .instances {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
    gap: var(--space-4);
  }

  /* ── card ─────────────────────────────────────────────────────────────── */
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
  .instance:hover {
    border-color: var(--color-accent, #4f86f7);
  }
  .instance.down {
    border-color: var(--color-error);
  }
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
  }
  .hostname {
    font-weight: var(--weight-semibold);
  }
  .icon-btn {
    background: transparent;
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    width: 24px;
    height: 22px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .icon-btn:hover {
    background: var(--color-surface-2);
    color: var(--color-text);
  }

  /* ── metrics ──────────────────────────────────────────────────────────── */
  .metrics {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  .metric-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--text-xs);
  }
  .load-row {
    align-items: baseline;
  }
  .metric-label {
    width: 36px;
    flex-shrink: 0;
    color: var(--color-text-dim);
    text-transform: uppercase;
    font-size: 10px;
    letter-spacing: 0.06em;
  }
  .bar-wrap {
    flex: 1;
    height: 6px;
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    overflow: hidden;
  }
  .bar {
    height: 100%;
    background: var(--color-accent, #4f86f7);
    border-radius: 3px;
    transition: width 0.3s ease;
  }
  .bar.warn {
    background: #e6a817;
  }
  .bar.crit {
    background: var(--color-error);
  }
  .metric-val {
    width: 120px;
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .load-val {
    width: auto;
    font-variant-numeric: tabular-nums;
  }
  .load-legend {
    font-size: 9px;
    letter-spacing: 0.04em;
    opacity: 0.6;
  }
  .gpu-val {
    display: flex;
    align-items: center;
    gap: 4px;
    overflow: hidden;
  }
  .gpu-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 80px;
  }
  .dim {
    color: var(--color-text-dim);
  }

  /* ── card footer ──────────────────────────────────────────────────────── */
  .card-footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
    gap: var(--space-2);
    margin-top: auto;
  }
  .primary-url {
    font-family: var(--font-mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .details-hint {
    flex-shrink: 0;
    color: var(--color-accent, #4f86f7);
    font-size: 10px;
    letter-spacing: 0.04em;
  }

  /* ── drawer ───────────────────────────────────────────────────────────── */
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.4);
    z-index: 100;
  }
  .drawer {
    position: fixed;
    top: 0;
    right: 0;
    bottom: 0;
    width: min(420px, 90vw);
    background: var(--color-surface);
    border-left: 1px solid var(--color-border);
    z-index: 101;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .drawer-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-4);
    border-bottom: 1px solid var(--color-border);
    flex-shrink: 0;
  }
  .drawer-body {
    flex: 1;
    overflow-y: auto;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }

  /* ── detail grid ──────────────────────────────────────────────────────── */
  dl.detail-grid,
  dl.addr-grid {
    margin: 0;
    display: grid;
    grid-template-columns: 80px 1fr;
    row-gap: 6px;
    column-gap: var(--space-3);
    font-size: var(--text-xs);
  }
  dl.addr-grid {
    grid-template-columns: 110px 1fr;
  }
  dt {
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: 10px;
    padding-top: 2px;
  }
  dd {
    margin: 0;
    color: var(--color-text);
    word-break: break-all;
  }
  code {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
    font-size: var(--text-xs);
  }

  .section-head {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-dim);
    border-bottom: 1px solid var(--color-border);
    padding-bottom: var(--space-1);
  }

  /* ── trust ────────────────────────────────────────────────────────────── */
  .trust-row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    font-size: var(--text-xs);
  }
  .trust-label {
    width: 48px;
    flex-shrink: 0;
    color: var(--color-text-dim);
    text-transform: uppercase;
    font-size: 10px;
    letter-spacing: 0.06em;
  }
  .trust-btn {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    color: var(--color-text-muted);
    font-size: var(--text-xs);
    padding: 2px 10px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
  }
  .trust-btn.active {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 15%, transparent);
    border-color: var(--color-accent, #4f86f7);
    color: var(--color-accent, #4f86f7);
  }
  .trust-btn:hover:not(:disabled) {
    border-color: var(--color-accent, #4f86f7);
    color: var(--color-text);
  }
  .trust-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .trust-readonly {
    color: var(--color-text-muted);
    font-style: italic;
  }

  .err {
    color: var(--color-error);
    font-size: var(--text-xs);
    font-family: var(--font-mono);
  }

  .hint {
    color: var(--color-text-dim);
    font-size: var(--text-xs);
    margin: 0;
  }

  /* ── update controls ──────────────────────────────────────────────────── */
  .update-controls {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .update-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .channel-select {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm, 4px);
    color: var(--color-text);
    font-size: var(--text-xs);
    padding: 3px 6px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .ctrl-btn {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    color: var(--color-text-muted);
    font-size: var(--text-xs);
    padding: 3px 10px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
    white-space: nowrap;
  }
  .ctrl-btn:hover:not(:disabled) {
    border-color: var(--color-accent, #4f86f7);
    color: var(--color-text);
  }
  .ctrl-btn.primary {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 15%, transparent);
    border-color: var(--color-accent, #4f86f7);
    color: var(--color-accent, #4f86f7);
  }
  .ctrl-btn.primary:hover:not(:disabled) {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 25%, transparent);
    color: var(--color-text);
  }
  .ctrl-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .update-status {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
    font-family: var(--font-mono);
  }
  .update-status.ok {
    color: var(--color-success, #4caf50);
  }
</style>
