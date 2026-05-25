<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { callTool } from '$lib/stores/runTool';
  import { notifications } from '$lib/stores/notifications';
  import StatusDot from '$lib/components/StatusDot.svelte';
  import Popover from '$lib/components/Popover.svelte';
  import PairingModal from '$lib/components/PairingModal.svelte';
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
    updateAvailable: boolean;
    updateLatest: string | null;
    pinnedTo: string | null;
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
  let retentionDays = $state(1);
  let customPopoverOpen = $state(false);
  let customDaysInput = $state('');
  let retentionSaving = $state(false);
  let pairModalOpen = $state(false);
  let pairModalMode = $state<'invite' | 'accept'>('accept');
  let pairModalInitialCode = $state('');

  type InboundOffer = {
    offer_id: string;
    peer_hostname: string;
    peer_addr: string;
    peer_port: number;
    inviter_peer_id?: string | null;
    expires_at: number;
    ttl_secs: number;
  };
  let inboundOffers = $state<InboundOffer[]>([]);

  async function refreshInboundOffers() {
    try {
      const rows = await callTool<InboundOffer[]>('systemPeerHandshakeList', {});
      inboundOffers = (rows ?? []).filter((r) => r.expires_at > Math.floor(Date.now() / 1000));
    } catch {
      // best-effort; this banner is informational
    }
  }

  function openPair(mode: 'invite' | 'accept', code = '') {
    pairModalMode = mode;
    pairModalInitialCode = code;
    pairModalOpen = true;
  }
  let pollHandle: ReturnType<typeof setInterval> | null = null;

  // Drawer update controls — reset only when the SELECTED INSTANCE changes,
  // not on every poll tick that updates instance data.
  let drawerVersionInput = $state('');
  let drawerOpenedForId = $state<string | null>(null);
  let updateResult = $state<{ done: string[]; errors: string[] } | null>(null);
  let updatePending = $state(false);
  let secureToggling = $state(false);
  // One popover per channel button. Bound via `popoverOpen[ch]` in the loop;
  // only one is open at a time (each opener closes the others first).
  let popoverOpen = $state<Record<string, boolean>>({ stable: false, rc: false, dev: false });

  // 1-second live poll; DB writes happen every 10 s (host_status_writer)
  const POLL_MS = 1000;

  // Preset segments (Custom is always index 3)
  const RETENTION_PRESETS = [
    { label: 'No history', value: 0 },
    { label: '1 day', value: 1 },
    { label: '7 days', value: 7 },
    { label: 'Custom', value: -1 },
  ];

  let activeSegment = $derived(
    RETENTION_PRESETS.findIndex((p) => p.value === retentionDays) >= 0 &&
    RETENTION_PRESETS.findIndex((p) => p.value === retentionDays) < 3
      ? RETENTION_PRESETS.findIndex((p) => p.value === retentionDays)
      : 3,
  );

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
          target?: string | null;
          mode?: string | null;
          channel?: string | null;
          update_available?: boolean | null;
          update_latest?: string | null;
          pinned_to?: string | null;
          addresses?: { kind: string; value: string }[];
          system?: SystemInfoReport | null;
        }[]>('systemPeerList', {}),
        callTool<{ peer_id: string; system?: SystemInfoReport | null }[]>(
          'systemHostStatusList',
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
            version: p.version ?? null,
            target: p.target ?? null,
            mode: p.mode ?? null,
            channel: p.channel ?? null,
            updateAvailable: p.update_available ?? false,
            updateLatest: p.update_latest ?? null,
            pinnedTo: p.pinned_to ?? null,
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

  // Friendly label for the canonical system_type tag. Every detected host
  // gets a badge — the OS row still carries the version string.
  function systemTypeLabel(t: string): string {
    switch (t) {
      case 'unraid': return 'Unraid';
      case 'proxmox-ve': return 'Proxmox VE';
      case 'proxmox-backup-server': return 'Proxmox Backup Server';
      case 'truenas-scale': return 'TrueNAS Scale';
      case 'truenas-core': return 'TrueNAS Core';
      case 'macos': return 'macOS';
      case 'debian': return 'Debian';
      case 'alpine': return 'Alpine';
      case 'nixos': return 'NixOS';
      case 'linux': return 'Linux';
      default: return t;
    }
  }

  function capabilityLabel(c: string): string {
    switch (c) {
      case 'docker': return 'Docker';
      case 'vm-host': return 'VM host';
      case 'lxc-host': return 'LXC host';
      case 'backup-target': return 'Backup target';
      case 'gpu-nvidia': return 'NVIDIA GPU';
      case 'gpu-amd': return 'AMD GPU';
      case 'gpu-intel': return 'Intel GPU';
      default: return c;
    }
  }

  async function loadRetention() {
    try {
      const data = await callTool<{ row: { json: string } | null }>('systemConfigGet', {
        noun: 'host_status',
        name: 'retention_days',
      });
      if (data?.row) retentionDays = parseFloat(data.row.json) ?? 1;
    } catch {
      // default 1 day
    }
  }

  async function setRetention(days: number) {
    retentionSaving = true;
    try {
      await callTool('systemConfigSet', {
        noun: 'host_status',
        name: 'retention_days',
        json: String(days),
      });
      retentionDays = days;
    } catch (e) {
      console.warn('retention set failed:', e);
    } finally {
      retentionSaving = false;
    }
  }

  async function applyCustomRetention() {
    const days = parseInt(customDaysInput, 10);
    if (!Number.isFinite(days) || days < 1) return;
    customPopoverOpen = false;
    await setRetention(days);
  }

  function customBtnLabel(): string {
    if (activeSegment === 3 && retentionDays > 0) return `${retentionDays}d`;
    return 'Custom';
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
      drawerVersionInput = selectedInst.pinnedTo ?? '';
      updateResult = null;
      popoverOpen = { stable: false, rc: false, dev: false };
    }
  });

  async function applyChannelUpdate(channel: string) {
    if (!selectedInst) return;
    updatePending = true;
    updateResult = null;
    popoverOpen = { stable: false, rc: false, dev: false };
    try {
      const args: Record<string, unknown> = { version: channel };
      if (selectedInst.role === 'system') args.peer_id = selectedInst.peerId;
      const r = await callTool<{ done: string[]; skipped: string[]; errors: string[] }>(
        'systemUpdate',
        args,
      );
      updateResult = { done: r.done, errors: r.errors };
      // Optimistic: assume the requested channel applied; puller will reconcile.
      if (selectedInst) selectedInst.channel = channel;
      instances = [...instances];
      void (selectedInst.role === 'local' ? refreshLocal(selectedInst) : refreshPodPeers());
    } catch (e) {
      console.warn('update failed:', e);
      updateResult = { done: [], errors: [e instanceof Error ? e.message : String(e)] };
    } finally {
      updatePending = false;
    }
  }

  async function applyVersion() {
    if (!selectedInst) return;
    const v = drawerVersionInput.trim();
    if (!v) return;
    updatePending = true;
    updateResult = null;
    try {
      const args: Record<string, unknown> = { version: v };
      if (selectedInst.role === 'system') args.peer_id = selectedInst.peerId;
      const r = await callTool<{ done: string[]; skipped: string[]; errors: string[] }>(
        'systemUpdate',
        args,
      );
      updateResult = { done: r.done, errors: r.errors };
      await (selectedInst.role === 'local' ? refreshLocal(selectedInst) : refreshPodPeers());
    } catch (e) {
      console.warn('version apply failed:', e);
      updateResult = { done: [], errors: [e instanceof Error ? e.message : String(e)] };
    } finally {
      updatePending = false;
    }
  }

  async function toggleSecure(inst: Instance) {
    if (secureToggling) return;
    secureToggling = true;
    try {
      const next = !(inst.sys?.self_secure ?? false);
      const args: Record<string, unknown> = { self_secure: next };
      if (inst.role === 'system') args.peer_id = inst.peerId;
      const result = await callTool<{ self_secure: boolean }>('systemPodUpdate', args);
      // Optimistically apply the authoritative response from the tool. The
      // background puller only refreshes host_status every 60 s, so without
      // this patch the UI would lag a full sync tick before reflecting the
      // change (and the user assumes the click did nothing).
      if (inst.sys) {
        inst.sys = { ...inst.sys, self_secure: result.self_secure };
      } else {
        inst.sys = { self_secure: result.self_secure } as SystemInfoReport;
      }
      instances = [...instances];
      // Kick off a background refresh to reconcile with the source of truth.
      void (inst.role === 'local' ? refreshLocal(inst) : refreshPodPeers());
    } catch (e) {
      console.warn('self_secure toggle failed:', e);
    } finally {
      secureToggling = false;
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
      updateAvailable: false,
      updateLatest: null,
      pinnedTo: null,
      health: 'unknown',
      error: null,
      lastChecked: null,
      sys: null,
    };
    instances = [local];
    refreshLocal(local);
    refreshPodPeers();
    loadRetention();
    refreshInboundOffers();
    pollHandle = setInterval(() => {
      const loc = instances.find((i) => i.role === 'local');
      if (loc) refreshLocal(loc);
      refreshPodPeers();
      refreshInboundOffers();
    }, POLL_MS);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  // Track which instances we've already notified so we don't spam on every poll.
  const notifiedUpdates = new Set<string>();

  $effect(() => {
    for (const inst of instances) {
      if (inst.updateAvailable && !notifiedUpdates.has(inst.id)) {
        notifiedUpdates.add(inst.id);
        const name = inst.sys?.hostname ?? inst.label;
        const ver = inst.updateLatest ? ` (${inst.updateLatest})` : '';
        notifications.info(`${name} has an update available${ver}`);
      }
    }
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
    if (!sys?.mem_total_mb || sys?.mem_used_mb == null) return 0;
    return Math.min(100, (sys.mem_used_mb / sys.mem_total_mb) * 100);
  }

  function loadPct(sys: SystemInfoReport | null | undefined): number | null {
    if (sys?.load_avg_1 == null || !sys?.cpu_logical) return null;
    return Math.min(100, (sys.load_avg_1 / sys.cpu_logical) * 100);
  }

  function cpuPct(sys: SystemInfoReport | null | undefined): number | null {
    return sys?.cpu_usage_percent ?? null;
  }

  async function copyText(text: string) {
    await navigator.clipboard.writeText(text);
    notifications.info('Copied');
  }

  function fmtUptime(secs: number): string {
    if (secs < 3600) return `${Math.floor(secs / 60)}m`;
    if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
    return `${Math.floor(secs / 86400)}d`;
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
      <div class="pair-btns">
        <button class="pair-btn" onclick={() => openPair('invite')}>+ Invite host</button>
        <button class="pair-btn" onclick={() => openPair('accept')}>+ Pair with code</button>
      </div>
      <div
        class="retention-picker"
        title="Storage setting — controls how many days of metrics are kept on disk"
      >
        <span class="retention-label">Keep history</span>
        <div class="retention-segment" role="radiogroup" aria-label="Keep history">
          {#each RETENTION_PRESETS as preset, i}
            {#if i < 3}
              <button
                class="segment-btn"
                class:is-active={activeSegment === i}
                role="radio"
                aria-checked={activeSegment === i}
                disabled={retentionSaving}
                onclick={() => setRetention(preset.value)}
              >{preset.label}</button>
            {:else}
              <Popover bind:open={customPopoverOpen} align="end" width={200}>
                {#snippet trigger()}
                  <button
                    class="segment-btn segment-btn-custom"
                    class:is-active={activeSegment === 3}
                    aria-haspopup="dialog"
                    aria-expanded={customPopoverOpen}
                    disabled={retentionSaving}
                    onclick={() => {
                      customDaysInput = activeSegment === 3 ? String(retentionDays) : '';
                      customPopoverOpen = true;
                    }}
                  >{customBtnLabel()}</button>
                {/snippet}
                {#snippet children()}
                  <div class="custom-popover">
                    <p class="custom-popover-label">Days to keep</p>
                    <input
                      type="number"
                      min="1"
                      max="365"
                      placeholder="e.g. 14"
                      bind:value={customDaysInput}
                      class="custom-days-input"
                      onkeydown={(e) => e.key === 'Enter' && applyCustomRetention()}
                    />
                    <button
                      class="custom-apply-btn"
                      onclick={applyCustomRetention}
                      disabled={!customDaysInput || parseInt(customDaysInput) < 1}
                    >Apply</button>
                  </div>
                {/snippet}
              </Popover>
            {/if}
          {/each}
        </div>
      </div>
    </div>
    <p class="lede">Connected orca instances.</p>
  </header>

  {#if inboundOffers.length > 0}
    <div class="inbound-banner" role="status">
      {#each inboundOffers as o (o.offer_id)}
        <div class="inbound-row">
          <div>
            <strong>{o.peer_hostname}</strong> wants to add this host to a pod.
            <span class="dim">({o.peer_addr}:{o.peer_port})</span>
          </div>
          <button class="btn primary sm" onclick={() => openPair('accept')}>Accept</button>
        </div>
      {/each}
    </div>
  {/if}

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
            {#if inst.updateAvailable}
              <span class="update-badge" title="Update available: {inst.updateLatest ?? 'newer version'}">↑ {inst.updateLatest ?? 'update'}</span>
            {/if}
          </div>
          <button class="icon-btn" onclick={(e) => refresh(inst, e)} title="Refresh">↻</button>
        </div>

        {#if inst.sys}
          <div class="metrics">
            <div class="metric-row">
              <div class="metric-head">
                <span class="metric-label">CPU</span>
                <span class="metric-val">
                  {cpuPct(inst.sys) != null ? `${cpuPct(inst.sys)!.toFixed(1)}%` : '—'}
                </span>
              </div>
              <div class="bar-wrap">
                <div
                  class="bar"
                  style="width:{cpuPct(inst.sys) ?? 0}%"
                  class:warn={(cpuPct(inst.sys) ?? 0) > 70}
                  class:crit={(cpuPct(inst.sys) ?? 0) > 90}
                ></div>
              </div>
            </div>
            <div class="metric-row">
              <div class="metric-head">
                <span class="metric-label">RAM</span>
                <span class="metric-val">
                  {memPct(inst.sys).toFixed(1)}% <span class="dim">{fmtMb(inst.sys.mem_used_mb)} / {fmtMb(inst.sys.mem_total_mb)}</span>
                </span>
              </div>
              <div class="bar-wrap">
                <div
                  class="bar"
                  style="width:{memPct(inst.sys)}%"
                  class:warn={memPct(inst.sys) > 70}
                  class:crit={memPct(inst.sys) > 90}
                ></div>
              </div>
            </div>
            {#if inst.sys.load_avg_1 != null}
              {@const lp = loadPct(inst.sys)}
              <div class="metric-row">
                <div class="metric-head">
                  <span class="metric-label" title="Unix run-queue depth (processes waiting for CPU), normalized by core count">CPU Q</span>
                  <span class="metric-val">
                    {inst.sys.load_avg_1.toFixed(2)}<span class="dim">/{inst.sys.cpu_logical ?? '?'} <span class="load-legend">1m avg</span></span>
                  </span>
                </div>
                <div class="bar-wrap">
                  <div
                    class="bar"
                    style="width:{lp ?? 0}%"
                    class:warn={(lp ?? 0) > 70}
                    class:crit={(lp ?? 0) > 90}
                  ></div>
                </div>
              </div>
            {/if}
            {#if inst.sys.gpus?.length}
              {#each inst.sys.gpus as g}
                <div class="metric-row">
                  <div class="metric-head">
                    <span class="metric-label">GPU</span>
                    <span class="metric-val gpu-val">
                      {g.utilization_percent != null ? `${g.utilization_percent.toFixed(0)}%` : '—'} <span class="dim gpu-name">{g.name}</span>
                    </span>
                  </div>
                  <div class="bar-wrap">
                    <div
                      class="bar"
                      style="width:{g.utilization_percent ?? 0}%"
                      class:warn={(g.utilization_percent ?? 0) > 70}
                      class:crit={(g.utilization_percent ?? 0) > 90}
                    ></div>
                  </div>
                </div>
              {/each}
            {/if}
          </div>
          {@const rawStats = [
            inst.sys.cpu_usage_percent != null ? `cpu ${inst.sys.cpu_usage_percent.toFixed(0)}%` : null,
            inst.sys.mem_used_mb != null ? `mem ${fmtMb(inst.sys.mem_used_mb)}/${fmtMb(inst.sys.mem_total_mb)}` : null,
            inst.sys.load_avg_1 != null ? `load ${inst.sys.load_avg_1.toFixed(2)}` : null,
            inst.sys.system_uptime_secs != null ? `up ${fmtUptime(inst.sys.system_uptime_secs)}` : null,
          ].filter((x): x is string => x != null)}
          {#if rawStats.length > 0}
            <p class="stat-raw">{rawStats.join(' · ')}</p>
          {/if}
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
      or click <strong>+ Pair with code</strong> above and paste a code from
      <code>orca pod pair &lt;this-host&gt;</code> on the inviter.
    </p>
  {/if}
</section>

<PairingModal
  open={pairModalOpen}
  initialMode={pairModalMode}
  initialCode={pairModalInitialCode}
  onclose={() => (pairModalOpen = false)}
  onpaired={() => {
    refreshPodPeers();
    refreshInboundOffers();
  }}
/>

<!-- Drawer -->
{#if selectedInst}
  {@const sys = selectedInst.sys}
  {@const typeBadge = sys?.system_type ? systemTypeLabel(sys.system_type) : ''}
  {@const virtBadge = sys?.virtualization && sys.virtualization !== 'none' ? sys.virtualization : ''}
  {@const capBadges = (sys?.detected_capabilities ?? []).map(capabilityLabel)}
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
      {#if typeBadge || virtBadge || capBadges.length}
        <div class="badges">
          {#if typeBadge}<span class="badge type-badge">{typeBadge}</span>{/if}
          {#if virtBadge}<span class="badge virt-badge">{virtBadge}</span>{/if}
          {#each capBadges as cap}<span class="badge cap-badge">{cap}</span>{/each}
        </div>
      {/if}

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

      <div class="secure-row" title="When on, this host is authorized to receive encrypted secrets replicated from other pod members. Independent of pairing.">
        <div class="secure-row-text">
          <span class="secure-label">SECURE</span>
          <span class="secure-hint">Can accept secrets from other systems</span>
        </div>
        <button
          class="toggle-switch"
          class:on={selectedInst.sys?.self_secure}
          disabled={secureToggling}
          onclick={() => toggleSecure(selectedInst!)}
          aria-label="Toggle SECURE (self_secure)"
          role="switch"
          aria-checked={!!selectedInst.sys?.self_secure}
        ><span class="toggle-thumb"></span></button>
      </div>

      {#if selectedInst.role === 'system'}
        <div class="paired-line" title="Paired peers in the pod automatically exchange mesh certs. Use Unpair to revoke.">
          <span class="paired-check">✓</span>
          <span>Paired</span>
        </div>
      {/if}

      <div class="section-head">Update</div>
      <div class="update-controls">
        <div class="version-line">
          <span class="version-label">Current</span>
          <code>{selectedInst.version ?? '—'}</code>
          {#if selectedInst.updateAvailable && selectedInst.updateLatest}
            <span class="update-pill avail">→ {selectedInst.updateLatest}</span>
          {:else if selectedInst.version}
            <span class="update-pill ok">up to date</span>
          {/if}
        </div>

        {#if selectedInst.pinnedTo}
          <div class="version-line">
            <span class="version-label">Pinned</span>
            <code>{selectedInst.pinnedTo}</code>
          </div>
        {/if}

        <div class="update-setting-row">
          <span class="update-setting-label">Channel</span>
          <div class="channel-segment">
            {#each ['stable', 'rc', 'dev'] as ch}
              {@const isCurrent = (selectedInst.channel ?? 'stable') === ch}
              {@const upToDate = isCurrent && !selectedInst.updateAvailable && selectedInst.version}
              <Popover bind:open={popoverOpen[ch]} align="end" width={260}>
                {#snippet trigger()}
                  <button
                    class="channel-btn"
                    class:active={isCurrent}
                    aria-haspopup="dialog"
                    aria-expanded={popoverOpen[ch]}
                    disabled={updatePending}
                    onclick={() => {
                      popoverOpen = {
                        stable: false, rc: false, dev: false, [ch]: !popoverOpen[ch],
                      };
                    }}
                  >{ch}</button>
                {/snippet}
                {#snippet children()}
                  <div class="channel-confirm">
                    {#if isCurrent}
                      {#if selectedInst.updateAvailable && selectedInst.updateLatest}
                        <p class="channel-confirm-title">Update on <strong>{ch}</strong>?</p>
                        <p class="version-diff">
                          <code>{selectedInst.version ?? '—'}</code>
                          <span class="arrow">→</span>
                          <code class="next">{selectedInst.updateLatest}</code>
                        </p>
                      {:else if upToDate}
                        <p class="channel-confirm-title">Already on latest <strong>{ch}</strong>.</p>
                        <p class="version-diff"><code>{selectedInst.version}</code></p>
                      {:else}
                        <p class="channel-confirm-title">Re-check <strong>{ch}</strong>?</p>
                      {/if}
                    {:else}
                      <p class="channel-confirm-title">Switch to <strong>{ch}</strong> + update?</p>
                      <p class="version-diff">
                        <code>{selectedInst.version ?? '—'}</code>
                        <span class="muted">({selectedInst.channel ?? 'stable'})</span>
                        <span class="arrow">→</span>
                        <code class="next">latest {ch}</code>
                      </p>
                    {/if}
                    <div class="confirm-actions">
                      <button
                        class="ctrl-btn"
                        onclick={() => { popoverOpen[ch] = false; }}
                      >Cancel</button>
                      {#if !(isCurrent && upToDate)}
                        <button
                          class="ctrl-btn primary"
                          disabled={updatePending}
                          onclick={() => applyChannelUpdate(ch)}
                        >{updatePending ? 'Updating…' : (isCurrent ? 'Update' : 'Switch')}</button>
                      {/if}
                    </div>
                  </div>
                {/snippet}
              </Popover>
            {/each}
          </div>
        </div>

        <div class="update-setting-row">
          <span class="update-setting-label">Version</span>
          <div class="version-pick">
            <input
              type="text"
              class="version-input"
              placeholder="e.g. 0.0.5-rc.1"
              bind:value={drawerVersionInput}
              disabled={updatePending}
            />
            <button
              class="ctrl-btn"
              onclick={applyVersion}
              disabled={updatePending || !drawerVersionInput.trim()}
              title="Pin and apply this specific version"
            >Pin</button>
          </div>
        </div>

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

  .pair-btns {
    margin-left: auto;
    display: flex;
    gap: var(--space-2);
  }
  .pair-btn {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-surface);
    color: var(--color-text);
    font-size: var(--text-sm);
    cursor: pointer;
    white-space: nowrap;
  }
  .pair-btn:hover { background: var(--color-bg-hover, var(--color-surface)); }

  .inbound-banner {
    margin: 0 0 var(--space-4);
    padding: var(--space-3);
    background: var(--color-accent-subtle, color-mix(in srgb, var(--color-accent) 12%, transparent));
    border: 1px solid var(--color-accent);
    border-radius: var(--radius-md);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .inbound-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: var(--space-3);
    font-size: var(--text-sm);
  }
  .inbound-row .dim { color: var(--color-text-dim); font-family: var(--font-mono); margin-left: var(--space-1); }
  .inbound-row .btn { padding: var(--space-1) var(--space-3); font-size: var(--text-xs); border-radius: var(--radius-md); border: 1px solid var(--color-accent); cursor: pointer; }
  .inbound-row .btn.primary { background: var(--color-accent); color: var(--color-on-accent, #fff); }

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
    white-space: nowrap;
  }
  .retention-segment {
    display: flex;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 6px;
    padding: 3px;
    gap: 2px;
  }
  .segment-btn {
    flex: 1 1 0;
    min-width: 0;
    background: transparent;
    border: 1px solid transparent;
    color: var(--color-text-muted);
    font-size: var(--text-xs);
    padding: 4px 10px;
    cursor: pointer;
    white-space: nowrap;
    border-radius: 4px;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
  }
  .segment-btn:hover:not(:disabled):not(.is-active) {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .segment-btn.is-active {
    background: color-mix(in srgb, var(--color-accent) 12%, var(--color-surface));
    color: var(--color-accent);
    border-color: var(--color-accent);
    font-weight: 500;
  }
  .segment-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .custom-popover {
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .custom-popover-label {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .custom-days-input {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm, 4px);
    color: var(--color-text);
    font-size: var(--text-sm);
    padding: 4px 8px;
    width: 100%;
    box-sizing: border-box;
  }
  .custom-days-input:focus {
    outline: none;
    border-color: var(--color-accent, #4f86f7);
  }
  .custom-apply-btn {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 15%, transparent);
    border: 1px solid var(--color-accent, #4f86f7);
    border-radius: 4px;
    color: var(--color-accent, #4f86f7);
    font-size: var(--text-xs);
    padding: 4px 12px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
    align-self: flex-end;
  }
  .custom-apply-btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 25%, transparent);
    color: var(--color-text);
  }
  .custom-apply-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
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
    min-width: 0;
  }
  .update-badge {
    font-size: 10px;
    font-weight: 600;
    padding: 1px 6px;
    border-radius: 10px;
    background: color-mix(in srgb, #f59e0b 15%, transparent);
    color: #f59e0b;
    border: 1px solid color-mix(in srgb, #f59e0b 40%, transparent);
    white-space: nowrap;
    flex-shrink: 0;
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
  .bar-wrap {
    width: 100%;
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
    transition: width 0.3s ease;
  }
  .bar.warn {
    background: #e6a817;
  }
  .bar.crit {
    background: var(--color-error);
  }
  .load-legend {
    font-size: 9px;
    letter-spacing: 0.04em;
    opacity: 0.7;
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

  /* ── badges + paired line ─────────────────────────────────────────────── */
  .badges {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }
  .badge {
    font-size: 10px;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 10px;
    line-height: 1.4;
    white-space: nowrap;
    border: 1px solid var(--color-border);
    background: color-mix(in srgb, var(--color-surface, #1a1a2e) 80%, transparent);
    color: var(--color-text-dim);
  }
  .badge.type-badge {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 15%, transparent);
    color: var(--color-accent, #4f86f7);
    border-color: color-mix(in srgb, var(--color-accent, #4f86f7) 40%, transparent);
  }
  .badge.virt-badge {
    background: color-mix(in srgb, #a855f7 12%, transparent);
    color: #c084fc;
    border-color: color-mix(in srgb, #a855f7 35%, transparent);
  }
  .secure-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-3);
    margin: var(--space-3) 0;
    background: color-mix(in srgb, var(--color-surface, #1a1a2e) 80%, transparent);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md, 8px);
  }
  .secure-row-text {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
    flex: 1;
  }
  .secure-label {
    font-size: var(--text-xs);
    font-weight: 700;
    letter-spacing: 0.06em;
    color: var(--color-text);
  }
  .secure-hint {
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    line-height: 1.3;
  }
  .paired-line {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: var(--space-3) 0;
    font-size: 11px;
    color: var(--color-text-dim);
  }
  .paired-check {
    color: #22c55e;
    font-weight: 700;
  }
  .toggle-switch {
    position: relative;
    width: 44px;
    height: 24px;
    padding: 0;
    background: var(--color-border);
    border: 1px solid color-mix(in srgb, var(--color-border) 60%, transparent);
    border-radius: 12px;
    flex-shrink: 0;
    cursor: pointer;
    transition: background 0.15s;
  }
  .toggle-switch:hover:not(:disabled) {
    background: color-mix(in srgb, var(--color-border) 60%, var(--color-accent, #4f86f7));
  }
  .toggle-switch:focus-visible {
    outline: 2px solid var(--color-accent, #4f86f7);
    outline-offset: 2px;
  }
  .toggle-switch:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .toggle-switch.on {
    background: var(--color-accent, #4f86f7);
  }
  .toggle-thumb {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 18px;
    height: 18px;
    background: white;
    border-radius: 50%;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.3);
    transition: left 0.15s;
    pointer-events: none;
  }
  .toggle-switch.on .toggle-thumb {
    left: 22px;
  }
  .stat-raw {
    margin: var(--space-1) 0 0;
    font-size: 10px;
    color: var(--color-text-dim);
    font-family: var(--font-mono);
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
  .version-line {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-sm);
  }
  .version-label {
    font-size: var(--text-xs);
    color: var(--color-text-dim);
  }
  .update-pill {
    font-size: var(--text-xs);
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--color-border);
  }
  .update-pill.ok {
    color: var(--color-text-dim);
  }
  .update-pill.avail {
    color: var(--color-accent, #4ea1ff);
    border-color: var(--color-accent, #4ea1ff);
  }
  .version-pick {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .version-input {
    background: color-mix(in srgb, var(--color-bg) 60%, transparent);
    border: 1px solid var(--color-border);
    border-radius: 6px;
    padding: 4px 8px;
    color: inherit;
    font-family: var(--font-mono, monospace);
    font-size: var(--text-sm);
    flex: 1;
    min-width: 22ch;
  }
  .version-pick {
    width: 100%;
  }
  .channel-confirm {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
  }
  .channel-confirm-title {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--color-text);
  }
  .version-diff {
    margin: 0;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
    font-size: var(--text-xs);
  }
  .version-diff .arrow {
    color: var(--color-text-dim);
  }
  .version-diff .next {
    color: var(--color-accent, #4f86f7);
  }
  .version-diff .muted {
    color: var(--color-text-dim);
  }
  .confirm-actions {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
    margin-top: var(--space-1);
  }
  .version-input:focus {
    outline: none;
    border-color: var(--color-accent, #4ea1ff);
  }
  .update-setting-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .update-setting-label {
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    flex-shrink: 0;
  }
  .update-action-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .channel-segment {
    display: flex;
    background: color-mix(in srgb, var(--color-bg) 60%, transparent);
    border: 1px solid var(--color-border);
    border-radius: 6px;
    overflow: hidden;
  }
  .channel-btn {
    background: transparent;
    border: none;
    color: var(--color-text-muted);
    font-size: var(--text-xs);
    padding: 3px 10px;
    cursor: pointer;
    transition: color 0.15s, background 0.15s;
  }
  .channel-btn.active {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 18%, transparent);
    color: var(--color-accent, #4f86f7);
  }
  .channel-btn:hover:not(:disabled):not(.active) {
    color: var(--color-text);
  }
  .channel-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
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
