<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/stores';
  import { callTool } from '$lib/stores/runTool';
  import { notifications } from '$lib/stores/notifications';
  import StatusDot from '$lib/components/StatusDot.svelte';
  import Popover from '$lib/components/Popover.svelte';
  import PairingModal from '$lib/components/PairingModal.svelte';
  import Drawer from '$lib/components/Drawer.svelte';
  import type { GpuInfo, SystemInfoReport, PodPeerDto } from '$lib/client/types.gen';
  import type { PageData } from './$types';

  let { data }: { data: PageData } = $props();
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
    updateCheckedSecs: number | null;
    pinnedTo: string | null;
    health: 'up' | 'down' | 'unknown';
    error: string | null;
    lastChecked: number | null;
    secure?: { local: boolean; peer: boolean } | null;
    status?: string | null;
    addresses?: { kind: string; value: string }[] | null;
    sys?: SystemInfoReport | null;
    // Set after a successful peer-dispatched mutation. Polling refreshes
    // (refreshPodPeers) read from the local mesh cache, which lags behind
    // the peer's true state by one mesh sync. While this window is active,
    // preserve fields the action authoritatively changed.
    actionLockUntil?: number;
    // Full version list from this peer's `system.update {}` probe, kept
    // fresh by the page-level fan-out poll. Empty until the first probe
    // completes. The drawer reads from this directly so opening it never
    // needs a Refresh click.
    availableVersions?: VersionEntry[];
  }

  // Synchronous seed from load() — fully populated at first paint, so the
  // OLD page stays visible during navigation until the NEW page's data is
  // ready (no spinner, no data snap-in). Polling in onMount keeps these
  // fresh via the same refresh* functions used today.
  function seedInstancesFromLoad(): Instance[] {
    const now = Date.now();
    const members = data.peers.members ?? [];
    const joined = members.filter(
      (m): m is PodPeerDto => 'peer_id' in m && (m as PodPeerDto).status !== undefined,
    );
    const selfPeer = joined.find((p) => p.local);
    const remotePeers = joined.filter((p) => p.status === 'active' && !p.local);

    const localProbe = data.probes['local'];
    const ld = data.localDetail;
    const local: Instance = {
      id: 'local',
      peerId: 'local',
      label: 'local',
      origin: originForLocal(),
      port: 12000,
      role: 'local',
      version: localProbe?.current_version ?? ld?.version ?? null,
      target: ld?.target ?? null,
      mode: ld?.mode ?? null,
      channel: localProbe?.channel ?? ld?.channel ?? null,
      updateAvailable: localProbe?.update_available === true,
      updateLatest: localProbe?.latest ?? null,
      updateCheckedSecs: null,
      pinnedTo: localProbe?.pinned_to ?? ld?.pinned_to ?? null,
      health: data.localHealthy ? 'up' : 'down',
      error: null,
      lastChecked: now,
      sys: ld?.system ?? selfPeer?.system ?? null,
      availableVersions: (localProbe?.available_versions ?? []) as VersionEntry[],
    };

    const podRows: Instance[] = remotePeers.map((p) => {
      const probe = data.probes[p.peer_id];
      const version = probe?.current_version ?? p.version ?? null;
      const latest = probe?.latest ?? p.update_latest ?? null;
      return {
        id: `system:${p.peer_id}`,
        peerId: p.peer_id,
        label: p.hostname || p.peer_id,
        origin: `${p.addr}:${p.port}`,
        port: p.port,
        role: 'system' as const,
        version,
        target: p.target ?? null,
        mode: p.mode ?? null,
        channel: probe?.channel ?? p.channel ?? null,
        updateAvailable:
          probe?.update_available === true ||
          (probe?.update_available == null && p.update_available === true),
        updateLatest: latest,
        updateCheckedSecs: p.update_checked_secs ?? null,
        pinnedTo: probe?.pinned_to ?? p.pinned_to ?? null,
        health: p.status === 'active' ? 'up' : 'down',
        error: null,
        lastChecked: now,
        secure: { local: p.local_secure, peer: p.peer_secure },
        status: p.status,
        addresses: (p.addresses ?? []).map((a) => ({ kind: a.kind, value: a.value })),
        sys: p.system ?? null,
        availableVersions: (probe?.available_versions ?? []) as VersionEntry[],
      };
    });
    return [local, ...podRows];
  }

  function seedRetentionFromLoad(): number {
    const row = data.retention?.row;
    if (!row) return 1;
    const v = parseFloat(row.json);
    return Number.isFinite(v) ? v : 1;
  }

  function seedInboundOffersFromLoad(): InboundOffer[] {
    const members = data.peers.members ?? [];
    const now = Math.floor(Date.now() / 1000);
    return members
      .filter((m) => (m as { state?: string }).state === 'handshaking')
      .map((m) => m as unknown as InboundOffer)
      .filter((r) => r.expires_at > now);
  }

  let instances = $state<Instance[]>(seedInstancesFromLoad());
  let selectedInstId = $state<string | null>(null);
  let retentionDays = $state(seedRetentionFromLoad());
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
  let inboundOffers = $state<InboundOffer[]>(seedInboundOffersFromLoad());

  // Un-joined systems split into two buckets so the operator can act:
  //   candidates → mDNS-discovered, unclaimed orcas we can ADD (join)
  //   stale      → departed peers + orphan identities (machine_id churn,
  //                decommissioned hosts) we can REMOVE (pod forget)
  type Candidate = {
    pubkey_fp: string;
    peer_id: string | null;
    hostname: string;
    addr: string;
    port: number;
    can_invite: boolean;
  };
  type StaleRow = {
    peer_id: string;
    hostname: string;
    addr: string;
    port: number;
    reason: string;
    last_seen_at: number | null;
  };
  let candidates = $state<Candidate[]>([]);
  let staleRows = $state<StaleRow[]>([]);
  let joiningFp = $state<string | null>(null);
  let forgettingId = $state<string | null>(null);

  async function joinCandidate(c: Candidate) {
    if (joiningFp) return;
    joiningFp = c.pubkey_fp;
    try {
      await callTool('podJoin', { action: 'invite', addr: c.addr, port: c.port });
      notifications.info(`Invite sent to ${c.hostname || c.addr}`);
      await refreshPodPeers();
    } catch (e) {
      notifications.error(`Join failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      joiningFp = null;
    }
  }

  async function forgetPeer(s: StaleRow) {
    if (forgettingId) return;
    forgettingId = s.peer_id;
    try {
      const r = await callTool<{ rows_removed: number; notified: unknown[] }>('podForget', {
        peer_id: s.peer_id,
      });
      notifications.info(
        `Forgot ${s.hostname || s.peer_id} (${r.rows_removed} rows, ${r.notified.length} peers notified)`,
      );
      await refreshPodPeers();
    } catch (e) {
      notifications.error(`Forget failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      forgettingId = null;
    }
  }

  async function refreshInboundOffers() {
    try {
      type PodMember = { state: 'joined' | 'handshaking' | 'discovered' } & Record<string, unknown>;
      const list = await callTool<{ members: PodMember[] }>('podList', {});
      const rows = (list?.members ?? [])
        .filter((m): m is PodMember & InboundOffer => m.state === 'handshaking')
        .map((m) => m as unknown as InboundOffer);
      inboundOffers = rows.filter((r) => r.expires_at > Math.floor(Date.now() / 1000));
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
  let probeHandle: ReturnType<typeof setInterval> | null = null;

  // Drawer update controls — reset only when the SELECTED INSTANCE changes,
  // not on every poll tick that updates instance data.
  type VersionEntry = { tag: string; prerelease: boolean; published_at: string | null; is_current: boolean };
  let drawerVersionSelect = $state('');
  let drawerChannelSelect = $state('stable');

  function inferChannel(version: string | null | undefined, fallback: string | null | undefined): string {
    const v = version ?? '';
    if (/-dev/i.test(v)) return 'dev';
    if (/-rc/i.test(v)) return 'rc';
    if (v) return 'stable';
    return fallback ?? 'stable';
  }

  function instChannel(i: { version: string | null; pinnedTo?: string | null; channel?: string | null } | null): string {
    if (!i) return 'stable';
    return inferChannel(i.pinnedTo ?? i.version, i.channel);
  }
  let drawerVersions = $state<VersionEntry[]>([]);
  let drawerVersionsLoading = $state(false);
  let drawerOpenedForId = $state<string | null>(null);
  let updateResult = $state<{ notes: string[]; errors: string[] } | null>(null);
  let updatePending = $state(false);
  let secureToggling = $state(false);
  let popoverOpen = $state<Record<string, boolean>>({ stable: false, rc: false, dev: false });

  // 1-second live poll; DB writes happen every 10 s (host_status_writer)
  const POLL_MS = 1000;
  // Per-peer `system.update {}` fan-out cadence. One mesh call per peer
  // per tick — heavier than the 1 s pod.list pull, so we space it out.
  // 60 s matches the daemon-side periodic probe and is enough for "new
  // version landed on GitHub" detection without hammering the mesh.
  const PROBE_MS = 60000;

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

  // View mode: tree (default, indented by parent_peer_id) or table (flat).
  // Lives in the URL so refresh / share preserves the choice.
  let view = $derived(($page.url.searchParams.get('view') === 'table' ? 'table' : 'tree') as 'tree' | 'table');
  function setView(v: 'tree' | 'table') {
    const u = new URL($page.url);
    if (v === 'tree') u.searchParams.delete('view');
    else u.searchParams.set('view', v);
    goto(`${u.pathname}${u.search}`, { replaceState: true, keepFocus: true, noScroll: true });
  }

  // Depth-first ordering by parent_peer_id (from system.parent_peer_id).
  // Roots first (no parent or unknown parent), then children indented under
  // them. Local host is always a root. Cycles broken by visited set.
  // Collapsed-node set for the tree. Click ▾/▸ to toggle.
  let collapsed = $state<Set<string>>(new Set());
  function toggleCollapsed(peerId: string) {
    const next = new Set(collapsed);
    if (next.has(peerId)) next.delete(peerId);
    else next.add(peerId);
    collapsed = next;
  }

  let displayInstances = $derived.by(() => {
    const byPeer = new Map<string, Instance>();
    for (const i of instances) byPeer.set(i.peerId, i);
    if (view === 'table') {
      return instances.map((inst) => ({ inst, depth: 0, prefix: '', hasChildren: false }));
    }
    // Client-side parent inference: match each peer's interface MACs
    // against every other peer's `system.claims[].macs`. Falls back to
    // server-set `parent_peer_id` if a claim ever lands there directly.
    // No backend dependency — the data needed is already in pod.list.
    const macIndex = new Map<string, string>(); // mac -> claiming peerId
    for (const inst of instances) {
      for (const c of inst.sys?.claims ?? []) {
        for (const m of c.macs ?? []) {
          if (m) macIndex.set(m.toLowerCase(), inst.peerId);
        }
      }
    }
    function inferParent(inst: Instance): string | null {
      const fromServer = inst.sys?.parent_peer_id;
      if (fromServer && fromServer !== inst.peerId && byPeer.has(fromServer)) return fromServer;
      for (const iface of inst.sys?.interfaces ?? []) {
        if (!iface.mac) continue;
        const claimer = macIndex.get(iface.mac.toLowerCase());
        if (claimer && claimer !== inst.peerId && byPeer.has(claimer)) return claimer;
      }
      return null;
    }
    const childrenOf = new Map<string, Instance[]>();
    const roots: Instance[] = [];
    for (const inst of instances) {
      const parent = inferParent(inst);
      if (parent) {
        const arr = childrenOf.get(parent) ?? [];
        arr.push(inst);
        childrenOf.set(parent, arr);
      } else {
        roots.push(inst);
      }
    }
    // Sort children alphabetically by hostname so the tree is stable.
    for (const arr of childrenOf.values()) {
      arr.sort((a, b) => (a.sys?.hostname ?? a.label).localeCompare(b.sys?.hostname ?? b.label));
    }
    roots.sort((a, b) => {
      // Local host first, then alphabetic.
      if (a.role === 'local') return -1;
      if (b.role === 'local') return 1;
      return (a.sys?.hostname ?? a.label).localeCompare(b.sys?.hostname ?? b.label);
    });

    const out: { inst: Instance; depth: number; prefix: string; hasChildren: boolean }[] = [];
    const visited = new Set<string>();
    // ancestorLast[i] === true means the ancestor at depth i was the LAST
    // child of its own parent — meaning we render blank space in that
    // column, not a vertical pipe.
    const walk = (inst: Instance, depth: number, isLastChild: boolean, ancestorLast: boolean[]) => {
      if (visited.has(inst.peerId)) return;
      visited.add(inst.peerId);
      const prefix = ancestorLast.map((last) => (last ? '   ' : '│  ')).join('') +
        (depth === 0 ? '' : (isLastChild ? '└─ ' : '├─ '));
      const kids = childrenOf.get(inst.peerId) ?? [];
      out.push({ inst, depth, prefix, hasChildren: kids.length > 0 });
      if (collapsed.has(inst.peerId)) return;
      for (let i = 0; i < kids.length; i++) {
        walk(kids[i], depth + 1, i === kids.length - 1, [...ancestorLast, isLastChild]);
      }
    };
    for (let i = 0; i < roots.length; i++) {
      walk(roots[i], 0, i === roots.length - 1, []);
    }
    for (const inst of instances)
      if (!visited.has(inst.peerId)) out.push({ inst, depth: 0, prefix: '', hasChildren: false });
    return out;
  });

  function originForLocal(): string {
    if (typeof window === 'undefined') return '';
    return window.location.origin;
  }

  function addrKindLabel(kind: string): string {
    switch (kind) {
      case 'lan_v4': return 'LAN IPv4';
      case 'lan_v6': return 'LAN IPv6';
      case 'tailscale_v4': return 'Tailscale IPv4';
      case 'tailscale_v6': return 'Tailscale IPv6';
      case 'fqdn': return 'FQDN';
      default: return kind;
    }
  }

  // Reachable LAN addresses for the card footer. Both IPv4 and IPv6 when
  // available. FQDN and Tailscale addrs live in the drawer Addresses section.
  function reachableAddrs(inst: Instance): string[] {
    const port = inst.port;
    const addrs = inst.addresses ?? [];
    // Pod peers report lan_v4/lan_v6 channels; the local host doesn't go
    // through pod discovery but systemDetail reports its primary IPs.
    const v4 = addrs.find((a) => a.kind === 'lan_v4')?.value ?? inst.sys?.primary_ipv4 ?? undefined;
    const v6 = addrs.find((a) => a.kind === 'lan_v6')?.value ?? inst.sys?.primary_ipv6 ?? undefined;
    const out: string[] = [];
    if (v4) out.push(`${v4}:${port}`);
    if (v6) out.push(`[${v6}]:${port}`);
    if (out.length > 0) return out;
    // Fallbacks when no LAN addr is reported.
    if (inst.sys?.fqdn) return [`${inst.sys.fqdn}:${port}`];
    const isIp = /^\d+\.\d+\.\d+\.\d+$|^[0-9a-f:]+$/i.test(inst.label);
    if (!isIp && inst.role !== 'local') return [`${inst.label}:${port}`];
    return [inst.origin];
  }

  async function refreshLocal(inst: Instance) {
    try {
      const [healthRes, detail] = await Promise.all([
        fetch('/api/health', { credentials: 'include' }).catch(() => null),
        callTool('systemDetail', {}),
      ]);
      inst.health = healthRes && healthRes.ok ? 'up' : 'down';
      const s = detail as {
        version: string;
        target: string;
        frontend: string;
        mode?: string;
        channel?: string;
        pinned_to?: string;
        system?: SystemInfoReport | null;
      };
      inst.version = s.version ?? null;
      inst.target = s.target ?? null;
      inst.mode = s.mode ?? null;
      inst.channel = s.channel ?? null;
      inst.pinnedTo = s.pinned_to ?? null;
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
      type PodPeer = {
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
        update_checked_secs?: number | null;
        pinned_to?: string | null;
        addresses?: { kind: string; value: string }[];
        system?: SystemInfoReport | null;
      };
      type PodMember = { state: 'joined' | 'handshaking' | 'discovered' } & Partial<PodPeer>;
      const listResult = await callTool<{ members: PodMember[] }>('podList', {});
      const members = listResult?.members ?? [];
      const joined = members
        .filter((m) => m.state === 'joined')
        .map((m) => m as unknown as PodPeer);
      const peersResult: PodPeer[] = joined.filter((p) => p.status === 'active');

      // Identify "self" so we can hide this host's own mDNS echoes.
      const selfPeer = joined.find((p) => p.local);
      const ownHostname = (selfPeer?.hostname ?? '').toLowerCase();
      const activePeerIds = new Set(peersResult.map((p) => p.peer_id));

      // Stale: departed/inactive joined peers — removable.
      const staleFromJoined: StaleRow[] = joined
        .filter((p) => !p.local && p.status !== 'active')
        .map((p) => ({
          peer_id: p.peer_id,
          hostname: p.hostname ?? p.peer_id,
          addr: p.addr ?? '',
          port: p.port ?? 0,
          reason: 'departed',
          last_seen_at: null,
        }));

      type DiscRow = {
        state: 'discovered';
        pubkey_fp: string;
        peer_id: string | null;
        hostname: string;
        addr: string;
        port: number;
        discovery_state: string;
        can_invite: boolean;
        last_seen_at: number;
      };
      const discovered = members
        .filter((m) => m.state === 'discovered')
        .map((m) => m as unknown as DiscRow)
        // Drop live echoes of peers we're already paired with.
        .filter((d) => !(d.peer_id && activePeerIds.has(d.peer_id)));

      const nextCandidates: Candidate[] = [];
      const staleFromDiscovery: StaleRow[] = [];
      for (const d of discovered) {
        const isSelfEcho = (d.hostname ?? '').toLowerCase() === ownHostname && !!ownHostname;
        const unclaimed = d.discovery_state === 'unclaimed';
        if (unclaimed && !isSelfEcho) {
          nextCandidates.push({
            pubkey_fp: d.pubkey_fp,
            peer_id: d.peer_id,
            hostname: d.hostname,
            addr: d.addr,
            port: d.port,
            can_invite: d.can_invite,
          });
        } else if (d.peer_id) {
          // pod:<id> orphan, or this host's own stale unclaimed identity.
          staleFromDiscovery.push({
            peer_id: d.peer_id,
            hostname: d.hostname,
            addr: d.addr,
            port: d.port,
            reason: isSelfEcho ? 'stale self identity' : 'orphan',
            last_seen_at: d.last_seen_at,
          });
        }
      }
      candidates = nextCandidates;
      staleRows = [...staleFromJoined, ...staleFromDiscovery];

      const sysById = new Map<string, SystemInfoReport | null>();
      for (const p of joined) {
        sysById.set(p.peer_id, p.system ?? null);
      }

      const local = instances.find((i) => i.role === 'local');
      // Filter out the synthetic local-host row — it duplicates the LOCAL card.
      const now = Date.now();
      const existingById = new Map(instances.map((i) => [i.id, i] as const));
      const podRows: Instance[] = (peersResult ?? [])
        .filter((p) => !p.local)
        .map((p) => {
          const id = `system:${p.peer_id}`;
          const prev = existingById.get(id);
          const storedSys = sysById.get(p.peer_id) ?? null;
          const sys = p.system ?? storedSys;
          // Within the action-lock window, the local mesh cache hasn't
          // reconciled the peer's authoritative state yet — preserve fields
          // the just-completed action mutated.
          const locked = prev && prev.actionLockUntil && now < prev.actionLockUntil;
          return {
            id,
            peerId: p.peer_id,
            label: p.hostname || p.peer_id,
            origin: `${p.addr}:${p.port}`,
            port: p.port,
            role: 'system' as const,
            version: locked ? prev.version : (p.version ?? null),
            target: p.target ?? null,
            mode: p.mode ?? null,
            channel: locked ? prev.channel : (p.channel ?? null),
            updateAvailable: locked ? prev.updateAvailable : (p.update_available ?? false),
            updateLatest: locked ? prev.updateLatest : (p.update_latest ?? null),
            updateCheckedSecs: locked ? prev.updateCheckedSecs : (p.update_checked_secs ?? null),
            pinnedTo: locked ? prev.pinnedTo : (p.pinned_to ?? null),
            health: p.status === 'active' ? 'up' : 'down',
            error: null,
            lastChecked: now,
            secure: { local: p.local_secure, peer: p.peer_secure },
            status: p.status,
            addresses: (p.addresses ?? []).map((a) => ({ kind: a.kind, value: a.value })),
            sys,
            actionLockUntil: prev?.actionLockUntil,
          };
        });
      instances = local ? [local, ...podRows] : podRows;
      // Per-tick sampling for the open-drawer histograms. Only sample the
      // currently selected peer to keep the rolling window bounded; the
      // window resets when the user switches drawers.
      if (selectedInstId) {
        const inst = instances.find((i) => i.id === selectedInstId);
        if (inst) sampleDrawerMetrics(inst);
      }
    } catch (e) {
      console.warn('pod.list failed:', e);
    }
  }

  // ── Drawer metric history (CPU / RAM / GPU / processes) ──────────────────
  // Rolling sample window kept in browser memory for the currently-open
  // drawer. Capped at HIST_LEN points; oldest dropped on each new sample.
  // Not persisted — drawer close = history cleared. The cards keep their
  // own (separate) sparkline state.
  const HIST_LEN = 120;
  type Sample = { t: number; cpu: number | null; memPct: number | null; gpuPct: (number | null)[] };
  let histSamples = $state<Sample[]>([]);
  let histProcMap = $state<Map<number, { name: string; cpu: number[]; mem: number[] }>>(new Map());
  let pinnedPid = $state<number | null>(null);

  function resetDrawerHistory() {
    histSamples = [];
    histProcMap = new Map();
    pinnedPid = null;
  }

  function sampleDrawerMetrics(inst: Instance) {
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
    histSamples = [...histSamples, sample].slice(-HIST_LEN);

    // Per-process series: append a point for each pid seen this tick,
    // null-pad pids we've tracked previously but didn't see now (so the
    // chart shows the drop instead of leaving stale values).
    const seen = new Set<number>();
    for (const p of s.top_processes ?? []) {
      seen.add(p.pid);
      const prev = histProcMap.get(p.pid);
      const next = prev ?? { name: p.name, cpu: [], mem: [] };
      next.cpu = [...next.cpu, p.cpu_percent].slice(-HIST_LEN);
      next.mem = [...next.mem, p.mem_mb].slice(-HIST_LEN);
      next.name = p.name;
      histProcMap.set(p.pid, next);
    }
    for (const [pid, v] of histProcMap) {
      if (!seen.has(pid)) {
        v.cpu = [...v.cpu, NaN].slice(-HIST_LEN);
        v.mem = [...v.mem, NaN].slice(-HIST_LEN);
      }
    }
    histProcMap = new Map(histProcMap);
  }

  // Render a chart segment list — each contiguous run of finite values
  // becomes one `{ line, area }` pair (NaN values split into separate
  // segments so dropouts render as gaps, not interpolated lines).
  function chartSegments(vals: number[], W: number, H: number, vmax: number): { line: string; area: string }[] {
    const out: { line: string; area: string }[] = [];
    if (!vals.length) return out;
    const n = vals.length;
    let line = '';
    let area = '';
    let segStartX: number | null = null;
    let segLastX: number | null = null;
    const flush = () => {
      if (line) {
        out.push({ line: line.trim(), area: `${area} L ${segLastX!.toFixed(1)} ${H} L ${segStartX!.toFixed(1)} ${H} Z`.trim() });
      }
      line = ''; area = ''; segStartX = null; segLastX = null;
    };
    for (let i = 0; i < n; i++) {
      const v = vals[i];
      const x = (i / Math.max(1, n - 1)) * W;
      if (!Number.isFinite(v)) { flush(); continue; }
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
      const data = await callTool<{ row: { json: string } | null }>('configGet', {
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
      await callTool('configSet', {
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
      drawerVersionSelect = selectedInst.version ? `v${selectedInst.version}` : '';
      drawerChannelSelect = inferChannel(selectedInst.version, selectedInst.channel);
      drawerVersions = [];
      updateResult = null;
      // Hydrate from the already-loaded pod.list row. If the page-level
      // probe hasn't completed for this peer yet (cold drawer open early
      // in the session), fire an immediate one-shot probe so the version
      // dropdown never sits empty waiting for the next 60 s tick.
      resetDrawerHistory();
      hydrateDrawerFromInstance();
      if (!(selectedInst.availableVersions ?? []).length) {
        void probeUpdateState();
      }
    }
  });

  // Pull the drawer's update fields from the already-loaded `pod.list` row
  // for the selected instance. No network call.
  function hydrateDrawerFromInstance() {
    if (!selectedInst) return;
    // Use whatever the page-level probe already fetched. If the first
    // probe hasn't completed yet this is empty, but the next tick of
    // probeAllInstances will mirror its result into `drawerVersions`.
    drawerVersions = selectedInst.availableVersions ?? [];
  }

  type SystemUpdateResp = {
    current_version: string;
    channel: string;
    pinned_to: string | null;
    dev_source: string | null;
    available_versions: VersionEntry[];
    latest: string | null;
    applied: string | null;
    hostname: string | null;
    fqdn: string | null;
    addressing_set: string[];
    os_package_result: string | null;
    notes: string[];
    errors: string[];
    // Server-computed (system::update_state::is_update_available) so list-
    // view (pod.list) and detail-view (system.update) always agree on the
    // same peer. Older peers may omit it — treat undefined as false.
    update_available?: boolean | null;
  };

  let detailRefreshing = $state(false);

  // Force a fresh `system.detail` call against the selected peer. For remote
  // peers the periodic `peer_detail_probe` keeps `inst.sys` warm via pod.list,
  // so users normally don't need this; it's here parity with the Update
  // section's Refresh for cases where the drawer needs immediate hydration.
  async function refreshDetail() {
    if (!selectedInst) return;
    detailRefreshing = true;
    try {
      const peer = selectedInst.role === 'system' ? selectedInst.peerId : null;
      const s = await callTool<{
        version: string;
        target: string;
        mode?: string;
        channel?: string;
        pinned_to?: string;
        system?: SystemInfoReport | null;
      }>('systemDetail', {}, { peer });
      if (selectedInst) {
        selectedInst.version = s.version ?? selectedInst.version;
        selectedInst.target = s.target ?? selectedInst.target;
        selectedInst.mode = s.mode ?? selectedInst.mode;
        selectedInst.channel = s.channel ?? selectedInst.channel;
        selectedInst.pinnedTo = s.pinned_to ?? selectedInst.pinnedTo;
        selectedInst.sys = s.system ?? selectedInst.sys;
        selectedInst.lastChecked = Date.now();
        instances = [...instances];
      }
    } catch (e) {
      console.warn('system.detail refresh failed:', e);
    } finally {
      detailRefreshing = false;
    }
  }

  async function probeUpdateState() {
    if (!selectedInst) return;
    drawerVersionsLoading = true;
    try {
      // READ-ONLY probe: empty args. Routing to the selected peer happens
      // via the `X-Orca-Peer` header (see callTool); the body MUST stay
      // empty so the server treats this as a state read, not a mutation.
      const peer = selectedInst.role === 'system' ? selectedInst.peerId : null;
      const r = await callTool<SystemUpdateResp>('systemUpdate', {}, { peer });
      drawerVersions = r.available_versions ?? [];
      if (selectedInst) {
        selectedInst.channel = r.channel;
        selectedInst.pinnedTo = r.pinned_to;
        selectedInst.actionLockUntil = Date.now() + 15000;
        if (r.current_version) selectedInst.version = r.current_version;
        if (r.current_version) drawerVersionSelect = `v${r.current_version}`;
        // Channel pill reflects the RUNNING binary, not the stored pref.
        // A host pinned to rc.9 should show "rc" even if its channel marker
        // was never written (defaults to stable). Server now does the same
        // for `latest`, but we recompute here too so the UI doesn't depend
        // on probe ordering.
        drawerChannelSelect = inferChannel(r.current_version, r.channel);
        if (r.latest) {
          selectedInst.updateLatest = r.latest;
          selectedInst.updateAvailable = r.update_available === true;
        }
        instances = [...instances];
      }
      if (!drawerVersionSelect && r.current_version) {
        drawerVersionSelect = `v${r.current_version}`;
      }
    } catch (e) {
      console.warn('update state probe failed:', e);
    } finally {
      drawerVersionsLoading = false;
    }
  }

  async function runSystemUpdate(args: Record<string, unknown>) {
    if (!selectedInst) return;
    updatePending = true;
    updateResult = null;
    try {
      // Route to the peer via `X-Orca-Peer` header — `peer_id` is not a
      // field on `SystemUpdateArgs` and would be silently dropped if passed
      // in the body. See callTool() for how the header is plumbed.
      const peer = selectedInst.role === 'system' ? selectedInst.peerId : null;
      const r = await callTool<SystemUpdateResp>('systemUpdate', args, { peer });
      updateResult = { notes: r.notes ?? [], errors: r.errors ?? [] };
      drawerVersions = r.available_versions ?? drawerVersions;
      if (selectedInst) {
        selectedInst.channel = r.channel;
        selectedInst.pinnedTo = r.pinned_to;
        selectedInst.actionLockUntil = Date.now() + 15000;
        if (r.current_version) selectedInst.version = r.current_version;
        if (r.current_version) drawerVersionSelect = `v${r.current_version}`;
        drawerChannelSelect = inferChannel(r.current_version, r.channel);
        if (r.latest) {
          selectedInst.updateLatest = r.latest;
          selectedInst.updateAvailable = r.update_available === true;
        }
        instances = [...instances];
      }
      // Don't immediately refresh from polling sources — for `role==='system'`
      // (remote peer) the mesh hasn't propagated the new pin/channel/version
      // state back yet, so a refresh here clobbers the authoritative response
      // we just got from the peer itself. Polling will reconcile on its own.
    } catch (e) {
      console.warn('system update failed:', e);
      updateResult = { notes: [], errors: [e instanceof Error ? e.message : String(e)] };
    } finally {
      updatePending = false;
    }
  }

  async function applyChannelUpdate(channel: string) {
    await runSystemUpdate({ channel });
  }

  async function applyUpdateSelection() {
    // S1 semantics (2026-06-02): no separate pin/unpin args. The backend
    // derives pin state from the version arg — selecting non-latest pins,
    // selecting latest unpins, omitting version updates to latest + unpins.
    const args: Record<string, unknown> = {};
    if (drawerChannelSelect && drawerChannelSelect !== inferChannel(selectedInst?.version, selectedInst?.channel)) {
      args.channel = drawerChannelSelect;
    }
    if (drawerVersionSelect && drawerVersionSelect !== `v${selectedInst?.version ?? ''}`) {
      args.version = drawerVersionSelect;
    }
    if (Object.keys(args).length === 0) return;
    await runSystemUpdate(args);
  }

  async function toggleSecure(inst: Instance) {
    if (secureToggling) return;
    secureToggling = true;
    try {
      const next = !(inst.sys?.self_secure ?? false);
      const args: Record<string, unknown> = { self_secure: next };
      if (inst.role === 'system') args.peer_id = inst.peerId;
      const result = await callTool<{ self_secure: boolean }>('podUpdate', args);
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
    // Initial data already populated synchronously from `data` (load() in
    // +page.ts) — onMount only registers periodic refresh timers. The
    // refresh* callbacks continue to drive subsequent ticks imperatively
    // (chose imperative over invalidate() so polling reuses the existing
    // in-place patch-state logic without re-running load()).
    pollHandle = setInterval(() => {
      const loc = instances.find((i) => i.role === 'local');
      if (loc) refreshLocal(loc);
      refreshPodPeers();
      refreshInboundOffers();
    }, POLL_MS);
    // Per-peer system.update {} fan-out — slower cadence than pod.list
    // polling because every tick crosses the mesh to every peer. Keeps
    // updateAvailable / current_version / channel / pinnedTo fresh on every
    // card (and on any open drawer) without the operator needing to click.
    probeHandle = setInterval(() => {
      void probeAllInstances();
    }, PROBE_MS);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
    if (probeHandle) clearInterval(probeHandle);
  });

  // Fan `system.update {}` out to every instance (local + every paired
  // peer) in parallel. Each response updates that instance's
  // updateAvailable / updateLatest / current_version / channel / pinned_to
  // in place. Read-only by contract — the empty args body MUST stay empty.
  async function probeAllInstances() {
    const snapshot = instances.filter((i) => i.health !== 'down');
    await Promise.all(
      snapshot.map(async (inst) => {
        const peer = inst.role === 'system' ? inst.peerId : null;
        try {
          const r = await callTool<SystemUpdateResp>('systemUpdate', {}, { peer });
          const target = instances.find((i) => i.id === inst.id);
          if (!target) return;
          if (target.actionLockUntil && Date.now() < target.actionLockUntil) return;
          if (r.current_version) target.version = r.current_version;
          target.channel = r.channel ?? target.channel;
          target.pinnedTo = r.pinned_to ?? null;
          if (r.latest) {
            target.updateLatest = r.latest;
            target.updateAvailable = r.update_available === true;
          } else {
            target.updateAvailable = false;
          }
          target.availableVersions = r.available_versions ?? [];
          target.lastChecked = Date.now();
          // Mirror into the drawer's version select if the user is looking
          // at this peer right now — keeps the dropdown options live
          // without forcing a Refresh click.
          if (selectedInst && selectedInst.id === target.id) {
            drawerVersions = target.availableVersions;
            if (!drawerVersionSelect && r.current_version) {
              drawerVersionSelect = `v${r.current_version}`;
            }
          }
        } catch (e) {
          console.debug(`system.update probe failed for ${inst.label}:`, e);
        }
      }),
    );
    instances = [...instances];
  }

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

  <div class="view-toggle">
    <button class:active={view === 'tree'} onclick={() => setView('tree')}>Tree</button>
    <button class:active={view === 'table'} onclick={() => setView('table')}>Table</button>
  </div>

  <div class="instances" class:tree={view === 'tree'}>
    {#each displayInstances as { inst, depth, prefix, hasChildren } (inst.id)}
      {#if view === 'tree'}
        <div
          class="tree-row"
          class:down={inst.health === 'down'}
          onclick={() => goto(`/systems/${inst.peerId}`)}
          role="button"
          tabindex="0"
          onkeydown={(e) => e.key === 'Enter' && goto(`/systems/${inst.peerId}`)}
        >
          <span class="tree-prefix" aria-hidden="true">{prefix}</span>
          {#if hasChildren}
            <button
              class="tree-toggle"
              onclick={(e) => { e.stopPropagation(); toggleCollapsed(inst.peerId); }}
              title={collapsed.has(inst.peerId) ? 'Expand' : 'Collapse'}
            >{collapsed.has(inst.peerId) ? '▸' : '▾'}</button>
          {:else}
            <span class="tree-toggle-spacer" aria-hidden="true"></span>
          {/if}
          <StatusDot ok={inst.health === 'up' ? true : inst.health === 'down' ? false : null} />
          <span class="hostname">{inst.sys?.hostname ?? inst.label}</span>
          {#if inst.sys?.system_type}<span class="badge-sm">{inst.sys.system_type}</span>{/if}
          {#if inst.version}<span class="meta-sm">v{inst.version}</span>{/if}
          {#if inst.updateAvailable}
            <span class="update-badge" title="Update available: {inst.updateLatest ?? 'newer version'}">↑ {inst.updateLatest ?? 'update'}</span>
          {/if}
          <span class="tree-stats">
            CPU {cpuPct(inst.sys) != null ? `${cpuPct(inst.sys)!.toFixed(0)}%` : '—'} ·
            RAM {memPct(inst.sys).toFixed(0)}%
          </span>
        </div>
      {:else}
      <div
        class="instance"
        class:down={inst.health === 'down'}
        class:child={depth > 0}
        style:margin-left="{depth * 24}px"
        onclick={() => goto(`/systems/${inst.peerId}`)}
        role="button"
        tabindex="0"
        onkeydown={(e) => e.key === 'Enter' && goto(`/systems/${inst.peerId}`)}
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
        {/if}

        <div class="card-footer">
          <span class="details-hint">Details →</span>
        </div>
      </div>
      {/if}
    {/each}
  </div>

  {#if instances.filter((i) => i.role === 'system').length === 0}
    <p class="hint">
      No paired systems yet. Run <code>orca pod init</code> to become a founder,
      or click <strong>+ Pair with code</strong> above and paste a code from
      <code>orca pod pair &lt;this-host&gt;</code> on the inviter.
    </p>
  {/if}

  {#if candidates.length > 0}
    <div class="aux-section">
      <div class="aux-head">Discovered — not yet joined</div>
      <div class="aux-list">
        {#each candidates as c (c.pubkey_fp)}
          <div class="aux-row">
            <div class="aux-ident">
              <StatusDot ok={null} />
              <span class="aux-name">{c.hostname || c.addr}</span>
              <span class="dim">{c.addr}:{c.port}</span>
            </div>
            <button
              class="btn primary sm"
              disabled={joiningFp === c.pubkey_fp}
              onclick={() => joinCandidate(c)}
            >{joiningFp === c.pubkey_fp ? 'Adding…' : '+ Add'}</button>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  {#if staleRows.length > 0}
    <div class="aux-section">
      <div class="aux-head">Dead / stale — safe to remove</div>
      <div class="aux-list">
        {#each staleRows as s (s.peer_id)}
          <div class="aux-row">
            <div class="aux-ident">
              <StatusDot ok={false} />
              <span class="aux-name">{s.hostname || s.peer_id}</span>
              <span class="dim">{s.addr}{s.port ? `:${s.port}` : ''}</span>
              <span class="aux-tag">{s.reason}</span>
            </div>
            <button
              class="btn danger sm"
              disabled={forgettingId === s.peer_id}
              onclick={() => forgetPeer(s)}
            >{forgettingId === s.peer_id ? 'Removing…' : 'Forget'}</button>
          </div>
        {/each}
      </div>
    </div>
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

{#snippet chartCell(label: string, vals: number[], vmax: number, unit: string, color: string)}
  {@const W = 400}
  {@const H = 90}
  {@const segs = chartSegments(vals, W, H, vmax)}
  {@const last = [...vals].reverse().find(Number.isFinite) ?? null}
  {@const lastStr = last == null ? '—' : unit === '%' ? `${last.toFixed(1)}%` : last < 1024 ? `${Math.round(last)} ${unit}` : `${(last / 1024).toFixed(1)} G${unit}`}
  <div class="hist-cell">
    <div class="hist-label">{label}<span class="hist-val">{lastStr}</span></div>
    <svg class="hist-svg" viewBox="0 0 {W} {H}" preserveAspectRatio="none" style="color: {color};">
      <!-- gridlines at 0, 25, 50, 75, 100% of vmax -->
      {#each [0.25, 0.5, 0.75] as g}
        <line x1="0" x2={W} y1={H * (1 - g)} y2={H * (1 - g)} stroke="currentColor" stroke-width="0.5" opacity="0.15" />
      {/each}
      {#each segs as s}
        <path d={s.area} fill="currentColor" opacity="0.18" />
        <path d={s.line} fill="none" stroke="currentColor" stroke-width="1.5" />
      {/each}
    </svg>
    <div class="hist-axis"><span>0</span><span>{unit === '%' ? '100%' : vmax < 1024 ? `${Math.round(vmax)} ${unit}` : `${(vmax / 1024).toFixed(1)} G${unit}`}</span></div>
  </div>
{/snippet}

<!-- Drawer -->
<Drawer open={!!selectedInst} side="right" onclose={closeDrawer} ariaLabel="Host details">
  {#if selectedInst}
    {@const sys = selectedInst.sys}
    {@const typeBadge = sys?.system_type ? systemTypeLabel(sys.system_type) : ''}
    {@const virtBadge = sys?.virtualization && sys.virtualization !== 'none' ? sys.virtualization : ''}
    {@const capBadges = (sys?.detected_capabilities ?? []).map(capabilityLabel)}
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
      <div style="display:flex;gap:6px;align-items:center;">
        <button
          class="ctrl-btn"
          style="font-size:11px; padding:2px 8px;"
          onclick={refreshDetail}
          disabled={detailRefreshing}
          title="Force a fresh system.detail probe of this peer"
        >{detailRefreshing ? 'Refreshing…' : 'Refresh'}</button>
        <button class="icon-btn" onclick={closeDrawer} title="Close">✕</button>
      </div>
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

      <div class="section-head">Live <span style="margin-left:8px; opacity:0.5; font-weight:400; font-size:11px;">{histSamples.length}/{HIST_LEN} samples</span></div>
      <div class="hist-grid">
        {@render chartCell('CPU', histSamples.map(s => s.cpu ?? NaN), 100, '%', '#89b4fa')}
        {@render chartCell('RAM', histSamples.map(s => s.memPct ?? NaN), 100, '%', '#a6e3a1')}
        {#each selectedInst.sys?.gpus ?? [] as g, gi}
          {@render chartCell(g.name || `GPU ${gi}`, histSamples.map(s => s.gpuPct?.[gi] ?? NaN), 100, '%', '#f5c2e7')}
        {/each}
      </div>

      {#if (selectedInst.sys?.top_processes ?? []).length}
        <div class="section-head">Top processes
          <span style="margin-left:8px; opacity:0.6; font-weight:400;">click to pin</span>
        </div>
        <table class="proc-table">
          <thead><tr><th>name</th><th>pid</th><th>cpu</th><th>mem</th></tr></thead>
          <tbody>
            {#each selectedInst.sys?.top_processes ?? [] as p (p.pid)}
              <tr class:pinned={pinnedPid === p.pid} onclick={() => { pinnedPid = pinnedPid === p.pid ? null : p.pid; }}>
                <td><code>{p.name}</code></td>
                <td><code>{p.pid}</code></td>
                <td>{p.cpu_percent.toFixed(1)}%</td>
                <td>{p.mem_mb < 1024 ? `${p.mem_mb} MB` : `${(p.mem_mb / 1024).toFixed(1)} GB`}</td>
              </tr>
            {/each}
          </tbody>
        </table>
        {#if pinnedPid != null && histProcMap.get(pinnedPid)}
          {@const pinned = histProcMap.get(pinnedPid)!}
          {@const maxMem = Math.max(1, ...pinned.mem.filter(Number.isFinite))}
          <div class="hist-grid">
            {@render chartCell(`${pinned.name} CPU`, pinned.cpu, 100, '%', '#fab387')}
            {@render chartCell(`${pinned.name} RAM`, pinned.mem, maxMem, 'MB', '#cba6f7')}
          </div>
        {/if}
      {/if}

      {#if (selectedInst.addresses ?? []).length > 0}
        <div class="section-head">Addresses</div>
        <dl class="addr-grid">
          {#each selectedInst.addresses ?? [] as a (a.kind + ':' + a.value)}
            <dt>{addrKindLabel(a.kind)}</dt>
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

      <div class="section-head">
        Update
        <button
          class="ctrl-btn"
          style="margin-left:8px; font-size:11px; padding:2px 8px;"
          onclick={probeUpdateState}
          disabled={drawerVersionsLoading || updatePending}
          title="Re-probe this peer's update state"
        >{drawerVersionsLoading ? 'Probing…' : 'Refresh'}</button>
      </div>
      <div class="update-controls">
        <div class="update-setting-row">
          <span class="update-setting-label">
            Version
            {#if selectedInst.pinnedTo}
              <span class="pin-badge" title={`Pinned to ${selectedInst.pinnedTo} — unpin to follow latest on channel`}>📌</span>
            {/if}
          </span>
          <select
            class="version-input"
            bind:value={drawerVersionSelect}
            disabled={updatePending}
          >
            {#if selectedInst.version && !drawerVersions.some((v) => v.tag === `v${selectedInst!.version}`)}
              <option value={`v${selectedInst.version}`}>v{selectedInst.version} (current)</option>
            {/if}
            {#each drawerVersions as v}
              <option value={v.tag}>{v.tag}{selectedInst.version && v.tag === `v${selectedInst.version}` ? ' (current)' : ''}</option>
            {/each}
          </select>
        </div>

        <div class="update-setting-row">
          <span class="update-setting-label">Channel</span>
          <div class="channel-segment">
            {#each ['stable', 'rc', 'dev'] as ch}
              <button
                class="channel-btn"
                class:active={drawerChannelSelect === ch}
                disabled={updatePending}
                onclick={() => (drawerChannelSelect = ch)}
                title={`Select ${ch} channel`}
              >{ch}</button>
            {/each}
          </div>
        </div>

        {#if !selectedInst.pinnedTo && selectedInst.updateAvailable && selectedInst.updateLatest}
          <p class="pinned-hint avail">Update available: <code>{selectedInst.updateLatest}</code></p>
        {/if}

        <div class="update-actions-row">
          <button
            class="ctrl-btn primary"
            onclick={applyUpdateSelection}
            disabled={updatePending || (!selectedInst.pinnedTo && `v${selectedInst.version ?? ''}` === drawerVersionSelect && inferChannel(selectedInst.version, selectedInst.channel) === drawerChannelSelect)}
            title="Apply selected channel and version — selecting a non-latest version pins; selecting latest unpins"
          >{updatePending ? 'Updating…' : 'Apply'}</button>
        </div>

        {#if updateResult}
          {#if updateResult.notes.length > 0}
            <p class="update-status ok">{updateResult.notes.join(' · ')}</p>
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
  {/if}
</Drawer>

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

  /* ── auxiliary system sections (discovered candidates + stale rows) ── */
  .aux-section {
    margin-top: var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    overflow: hidden;
  }
  .aux-head {
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface);
    color: var(--color-text-dim);
    font-size: var(--text-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    border-bottom: 1px solid var(--color-border);
  }
  .aux-list { display: flex; flex-direction: column; }
  .aux-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-border);
  }
  .aux-row:last-child { border-bottom: none; }
  .aux-ident { display: flex; align-items: center; gap: var(--space-2); font-size: var(--text-sm); }
  .aux-name { font-weight: var(--weight-semibold); }
  .aux-ident .dim { color: var(--color-text-dim); font-family: var(--font-mono); font-size: var(--text-xs); }
  .aux-tag {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-text-dim);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
  }
  .btn.sm { padding: var(--space-1) var(--space-3); font-size: var(--text-xs); border-radius: var(--radius-md); border: 1px solid var(--color-border); background: var(--color-surface); color: var(--color-text); cursor: pointer; }
  .btn.sm:hover:not(:disabled) { background: var(--color-surface-2); }
  .btn.sm:disabled { opacity: 0.5; cursor: default; }
  .btn.primary.sm { background: var(--color-accent); color: var(--color-on-accent, #fff); border-color: var(--color-accent); }
  .btn.danger.sm { color: var(--color-error); border-color: var(--color-error); }
  .btn.danger.sm:hover:not(:disabled) { background: color-mix(in srgb, var(--color-error) 14%, transparent); }

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
  .instances.tree {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .tree-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--text-sm);
    color: var(--text);
  }
  .tree-row:hover { background: var(--surface); }
  .tree-row.down { opacity: 0.6; }
  .tree-prefix {
    font-family: var(--font-mono);
    color: var(--color-text-dim);
    white-space: pre;
    font-size: var(--text-sm);
    line-height: 1;
  }
  .tree-toggle {
    background: none;
    border: 0;
    color: var(--muted);
    cursor: pointer;
    padding: 0 var(--space-1);
    font-size: var(--text-sm);
    line-height: 1;
  }
  .tree-toggle:hover { color: var(--text); }
  .tree-toggle-spacer {
    display: inline-block;
    width: calc(var(--space-1) * 2 + 0.6em);
  }
  .tree-row .hostname { font-weight: var(--weight-medium); }
  .tree-stats {
    margin-left: auto;
    color: var(--muted);
    font-size: var(--text-xs);
    font-variant-numeric: tabular-nums;
  }
  .badge-sm {
    background: var(--code-bg);
    color: var(--muted);
    padding: 1px var(--space-2);
    border-radius: 999px;
    font-size: var(--text-xs);
  }
  .meta-sm { font-size: var(--text-xs); color: var(--muted); }
  .view-toggle {
    display: flex;
    gap: 0.25rem;
    margin-bottom: var(--space-3);
  }
  .view-toggle button {
    background: none;
    border: 1px solid var(--border-2, #444);
    color: inherit;
    padding: 0.2rem 0.7rem;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
  }
  .view-toggle button.active {
    background: var(--accent, #89b4fa);
    color: #11111b;
    border-color: transparent;
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
  .primary-urls {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
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

  .hist-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    gap: 8px;
    margin: 8px 0 12px;
  }
  .hist-cell {
    background: var(--bg-elevated, rgba(255, 255, 255, 0.03));
    border: 1px solid var(--border-subtle, rgba(255, 255, 255, 0.06));
    border-radius: 6px;
    padding: 8px;
    color: var(--accent, #89b4fa);
  }
  .hist-label {
    display: flex;
    justify-content: space-between;
    font-size: 11px;
    color: var(--text-secondary, rgba(255, 255, 255, 0.6));
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin-bottom: 4px;
  }
  .hist-val {
    color: var(--text-primary, #fff);
    font-family: ui-monospace, monospace;
    text-transform: none;
    letter-spacing: 0;
  }
  .hist-svg { display: block; width: 100%; height: 90px; }
  .hist-axis {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    color: var(--text-secondary, rgba(255, 255, 255, 0.45));
    font-family: ui-monospace, monospace;
    margin-top: 2px;
  }
  .proc-table {
    width: 100%;
    font-size: 12px;
    border-collapse: collapse;
    margin: 6px 0 12px;
  }
  .proc-table th, .proc-table td {
    padding: 4px 6px;
    text-align: left;
    border-bottom: 1px solid var(--border-subtle, rgba(255, 255, 255, 0.06));
  }
  .proc-table th { font-weight: 500; color: var(--text-secondary, rgba(255, 255, 255, 0.6)); }
  .proc-table tbody tr { cursor: pointer; }
  .proc-table tbody tr:hover { background: var(--bg-elevated, rgba(255, 255, 255, 0.04)); }
  .proc-table tr.pinned { background: rgba(137, 180, 250, 0.15); }
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
  .channel-btn.active:disabled {
    opacity: 1;
    cursor: default;
  }
  .update-actions-row {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
  .pinned-hint {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
  }
  .pinned-hint.avail {
    color: var(--color-accent, #4f86f7);
  }
  .pin-badge {
    font-size: var(--text-xs);
    padding: 2px 8px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 18%, transparent);
    color: var(--color-accent, #4f86f7);
    border: 1px solid color-mix(in srgb, var(--color-accent, #4f86f7) 40%, transparent);
    white-space: nowrap;
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
