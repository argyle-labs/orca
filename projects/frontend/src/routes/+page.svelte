<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/stores';
  import { callTool, unwrap } from '$lib/stores/runTool';
  import { proxmoxClusterList } from '$lib/client/sdk.gen';
  import { notifications } from '$lib/stores/notifications';
  import { createPoller } from '$lib/utils/polling';
  import { relTime, fmtMb, fmtUptime, fmtGpu } from '$lib/utils/format';
  import { memPct, loadPct, cpuPct } from '$lib/utils/sysMetrics';
  import { addrKindLabel, systemTypeLabel, capabilityLabel } from '$lib/utils/labels';
  import { inferChannel, instChannel } from '$lib/utils/version';
  import StatusDot from '$lib/components/StatusDot.svelte';
  import Popover from '$lib/components/Popover.svelte';
  import PairingModal from '$lib/components/PairingModal.svelte';
  import Drawer from '$lib/components/Drawer.svelte';
  import MetricRow from '$lib/components/MetricRow.svelte';
  import Chart from '$lib/components/Chart.svelte';
  import SegmentedControl from '$lib/components/SegmentedControl.svelte';
  import ToggleSwitch from '$lib/components/ToggleSwitch.svelte';
  import InstanceCard from '$lib/components/InstanceCard.svelte';
  import InstanceTreeRow from '$lib/components/InstanceTreeRow.svelte';
  import ClusterHeader from '$lib/components/ClusterHeader.svelte';
  import AuxList from '$lib/components/AuxList.svelte';
  import AuxRow from '$lib/components/AuxRow.svelte';
  import InboundOffersBanner from '$lib/components/InboundOffersBanner.svelte';
  import IconButton from '$lib/components/IconButton.svelte';
  import SectionHead from '$lib/components/SectionHead.svelte';
  import type {
    SystemInfoReport,
    PodPeerDto,
    ProxmoxClusterListEntry,
  } from '$lib/client/types.gen';
  import type { Instance, VersionEntry } from '$lib/types/instance';
  import type { PageData } from './$types';

  let { data }: { data: PageData } = $props();

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

  // ── Proxmox cluster grouping ──────────────────────────────────────────────
  // Phase E of the Proxmox vision: peers that belong to a Proxmox cluster
  // render under a cluster header in the systems tree. Sourced from the
  // backend tool `proxmox.cluster_list` which walks every enabled Proxmox
  // endpoint and reports its `/cluster/status` envelope. Matching is by IP
  // first (exact, taken from `ClusterNode.ip` against any of a peer's
  // `addresses[]` values), then falls back to case-insensitive hostname
  // against `ClusterNode.name`. Standalone Proxmox hosts (no cluster
  // configured) report `name: null` and are NOT grouped.
  type ClusterSummary = {
    name: string;
    quorate: boolean | null;
    online: number;
    total: number;
  };
  // `null` cluster = un-grouped bucket.
  let clusterByPeer = $state<Map<string, string | null>>(new Map());
  let clusterSummaries = $state<Map<string, ClusterSummary>>(new Map());

  async function refreshProxmoxClusters() {
    try {
      const list: ProxmoxClusterListEntry[] = await unwrap(proxmoxClusterList({ body: {} }));
      // Build IP and hostname indexes: ip|host → cluster name (only when
      // the endpoint actually reports a cluster — standalone hosts are
      // ignored so they fall through to the un-grouped bucket).
      const byIp = new Map<string, string>();
      const byHost = new Map<string, string>();
      const summaries = new Map<string, ClusterSummary>();
      for (const entry of list ?? []) {
        const cname = entry.status.name;
        if (!cname) continue;
        const total = entry.status.nodes.length;
        const online = entry.status.nodes.filter((n) => n.online === true).length;
        // Multiple endpoints can report the same cluster; prefer the
        // healthier reading (more online nodes seen).
        const prev = summaries.get(cname);
        if (!prev || online > prev.online) {
          summaries.set(cname, {
            name: cname,
            quorate: entry.status.quorate ?? null,
            online,
            total,
          });
        }
        for (const n of entry.status.nodes) {
          if (n.ip) byIp.set(n.ip, cname);
          if (n.name) byHost.set(n.name.toLowerCase(), cname);
        }
      }
      // Resolve each peer.
      const next = new Map<string, string | null>();
      for (const inst of instances) {
        let matched: string | null = null;
        for (const a of inst.addresses ?? []) {
          const hit = byIp.get(a.value);
          if (hit) { matched = hit; break; }
        }
        if (!matched && inst.sys?.primary_ipv4) {
          matched = byIp.get(inst.sys.primary_ipv4) ?? null;
        }
        if (!matched) {
          const host = (inst.sys?.hostname ?? inst.label ?? '').toLowerCase();
          if (host) matched = byHost.get(host) ?? null;
        }
        next.set(inst.peerId, matched);
      }
      clusterByPeer = next;
      clusterSummaries = summaries;
    } catch (e) {
      // No registered endpoints / daemon failure — leave existing
      // groupings in place. Log only; no toast (this poll is silent).
      console.warn('proxmox.cluster_list failed:', e);
    }
  }

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

  // Inbound offers are derived from the SAME `pod.list` payload that
  // `refreshPodPeers` already fetches — never make a second call for them.
  // Pass `members` from the existing fetch; if a caller has no list handy
  // (e.g. initial mount before the first refresh tick), it can pass null
  // and we skip — the next refreshPodPeers will populate.
  type PodMemberLite = { state: 'joined' | 'handshaking' | 'discovered' } & Record<string, unknown>;
  function applyInboundOffersFrom(members: PodMemberLite[] | null) {
    if (!members) return;
    const rows = members
      .filter((m): m is PodMemberLite & InboundOffer => m.state === 'handshaking')
      .map((m) => m as unknown as InboundOffer);
    inboundOffers = rows.filter((r) => r.expires_at > Math.floor(Date.now() / 1000));
  }

  function openPair(mode: 'invite' | 'accept', code = '') {
    pairModalMode = mode;
    pairModalInitialCode = code;
    pairModalOpen = true;
  }

  // Drawer update controls — reset only when the SELECTED INSTANCE changes,
  // not on every poll tick that updates instance data.
  let drawerVersionSelect = $state('');
  let drawerChannelSelect = $state('stable');

  let drawerVersions = $state<VersionEntry[]>([]);
  let drawerVersionsLoading = $state(false);
  let drawerOpenedForId = $state<string | null>(null);
  let updateResult = $state<{ notes: string[]; errors: string[] } | null>(null);
  let updatePending = $state(false);
  let secureToggling = $state(false);
  let popoverOpen = $state<Record<string, boolean>>({ stable: false, rc: false, dev: false });

  // List-view poll cadence. 5 s, NOT 1 s — the prior 1 s tick was firing
  // `refreshLocal` + `refreshPodPeers` + `refreshInboundOffers` every tick,
  // and the latter two BOTH called `pod.list`, so the daemon saw 4 calls/sec
  // sustained per open systems list (~240 calls/min, ~24 % daemon CPU on
  // mint 2026-06-15). Sub-second realtime is reserved for the FOCUSED
  // system-detail page (see [[project-realtime-system-telemetry-fast-ticks]])
  // — the list view trades a few seconds of staleness for not burning the
  // daemon. The eventual SSE/WS push design will replace this poll entirely.
  const POLL_MS = 5000;
  // Per-peer `system.update {}` fan-out cadence. One mesh call per peer
  // per tick — heavier than the pod.list pull, so we space it out.
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

  // Group displayInstances by Proxmox cluster for the tree view. Header
  // rows are synthetic — they carry no `inst` and short-circuit the tree
  // row rendering below. In `table` mode we skip grouping (the table is
  // a flat list by design). Ungrouped peers fall under a sentinel
  // "Ungrouped" header only when at least one cluster IS present —
  // otherwise we render the existing flat list unchanged (no point in a
  // single-section header when there are no clusters to contrast it
  // against).
  type DisplayRow =
    | { kind: 'header'; cluster: string | null; summary: ClusterSummary | null; key: string }
    | { kind: 'inst'; inst: Instance; depth: number; prefix: string; hasChildren: boolean; key: string };
  let displayRows = $derived.by<DisplayRow[]>(() => {
    if (view === 'table' || clusterSummaries.size === 0) {
      return displayInstances.map((r) => ({
        kind: 'inst' as const,
        inst: r.inst,
        depth: r.depth,
        prefix: r.prefix,
        hasChildren: r.hasChildren,
        key: `i:${r.inst.id}`,
      }));
    }
    // Bucket by cluster, preserving the existing ordering within each
    // bucket. Walk a sub-tree by tracking depth — once we see a root
    // (depth 0), the cluster of that root governs all subsequent
    // children until the next root.
    const buckets = new Map<string | null, typeof displayInstances>();
    let currentCluster: string | null = null;
    for (const row of displayInstances) {
      if (row.depth === 0) {
        currentCluster = clusterByPeer.get(row.inst.peerId) ?? null;
      }
      const arr = buckets.get(currentCluster) ?? [];
      arr.push(row);
      buckets.set(currentCluster, arr);
    }
    // Render order: named clusters alphabetically, then Ungrouped last.
    const named = [...buckets.keys()]
      .filter((k): k is string => k !== null)
      .sort((a, b) => a.localeCompare(b));
    const ordered: (string | null)[] = [...named, ...(buckets.has(null) ? [null] : [])];
    const out: DisplayRow[] = [];
    for (const cname of ordered) {
      const summary = cname ? (clusterSummaries.get(cname) ?? null) : null;
      out.push({
        kind: 'header',
        cluster: cname,
        summary,
        key: `h:${cname ?? '__ungrouped__'}`,
      });
      for (const row of buckets.get(cname) ?? []) {
        out.push({
          kind: 'inst',
          inst: row.inst,
          depth: row.depth,
          prefix: row.prefix,
          hasChildren: row.hasChildren,
          key: `i:${row.inst.id}`,
        });
      }
    }
    return out;
  });

  function originForLocal(): string {
    if (typeof window === 'undefined') return '';
    return window.location.origin;
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
      // Inbound offers piggy-back on this single pod.list — derived from
      // the same payload so we don't fire a second identical call.
      applyInboundOffersFrom(members as unknown as PodMemberLite[]);
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

  // Initial data already populated synchronously from `data` (load() in
  // +page.ts) — onMount only registers periodic refresh pollers. Polling
  // reuses the existing in-place patch-state logic instead of invalidate()
  // so we don't re-run load() on every tick.
  const listPoller = createPoller({
    intervalMs: POLL_MS,
    immediate: false, // load() already seeded the first frame
    fn: () => {
      const loc = instances.find((i) => i.role === 'local');
      if (loc) refreshLocal(loc);
      // refreshPodPeers fans out into applyInboundOffersFrom from the same
      // pod.list payload — no separate refreshInboundOffers tick needed.
      void refreshPodPeers();
    },
  });
  // Per-peer system.update {} fan-out — slower cadence than pod.list polling
  // because every tick crosses the mesh to every peer. Fires the first probe
  // pass immediately: load() no longer awaits the mesh fan-out, so this is
  // what populates version / channel / update-available on each row after
  // first paint.
  const probePoller = createPoller({ intervalMs: PROBE_MS, fn: probeAllInstances });
  // Proxmox cluster grouping for the systems tree. Membership changes are
  // rare and cluster.list walks every endpoint, so we share the slow probe
  // cadence rather than ticking every 5s.
  const clusterPoller = createPoller({ intervalMs: PROBE_MS, fn: refreshProxmoxClusters });

  onMount(() => {
    listPoller.start();
    probePoller.start();
    clusterPoller.start();
  });

  onDestroy(() => {
    listPoller.stop();
    probePoller.stop();
    clusterPoller.stop();
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

  async function copyText(text: string) {
    await navigator.clipboard.writeText(text);
    notifications.info('Copied');
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

  <InboundOffersBanner offers={inboundOffers} onaccept={() => openPair('accept')} />

  <SegmentedControl
    ariaLabel="View mode"
    items={[{ label: 'Tree', value: 'tree' }, { label: 'Table', value: 'table' }]}
    value={view}
    onchange={(v) => setView(v as 'tree' | 'table')}
  />

  <div class="instances" class:tree={view === 'tree'}>
    {#each displayRows as row (row.key)}
      {#if row.kind === 'header'}
        <ClusterHeader cluster={row.cluster} summary={row.summary} />
      {:else}
        {@const inst = row.inst}
        {#if view === 'tree'}
          <InstanceTreeRow
            {inst}
            prefix={row.prefix}
            hasChildren={row.hasChildren}
            collapsed={collapsed.has(inst.peerId)}
            onactivate={() => goto(`/systems/${inst.peerId}`)}
            ontoggle={() => toggleCollapsed(inst.peerId)}
          />
        {:else}
          <InstanceCard {inst} depth={row.depth} onactivate={() => goto(`/systems/${inst.peerId}`)} />
        {/if}
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
    <AuxList title="Discovered — not yet joined">
      {#each candidates as c (c.pubkey_fp)}
        <AuxRow
          name={c.hostname || c.addr}
          sub={`${c.addr}:${c.port}`}
          statusOk={null}
          actionLabel="+ Add"
          busyLabel="Adding…"
          busy={joiningFp === c.pubkey_fp}
          onaction={() => joinCandidate(c)}
        />
      {/each}
    </AuxList>
  {/if}

  {#if staleRows.length > 0}
    <AuxList title="Dead / stale — safe to remove">
      {#each staleRows as s (s.peer_id)}
        <AuxRow
          name={s.hostname || s.peer_id}
          sub={`${s.addr}${s.port ? `:${s.port}` : ''}`}
          tag={s.reason}
          statusOk={false}
          actionLabel="Forget"
          busyLabel="Removing…"
          busy={forgettingId === s.peer_id}
          actionVariant="danger"
          onaction={() => forgetPeer(s)}
        />
      {/each}
    </AuxList>
  {/if}
</section>

<PairingModal
  open={pairModalOpen}
  initialMode={pairModalMode}
  initialCode={pairModalInitialCode}
  onclose={() => (pairModalOpen = false)}
  onpaired={() => {
    // refreshPodPeers cascades into applyInboundOffersFrom — single call,
    // no duplicate pod.list.
    refreshPodPeers();
  }}
/>

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
        <IconButton onclick={closeDrawer} title="Close">✕</IconButton>
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

      <SectionHead title="Live">
        {#snippet trailing()}
          <span class="section-meta">{histSamples.length}/{HIST_LEN} samples</span>
        {/snippet}
      </SectionHead>
      <div class="hist-grid">
        <Chart label="CPU" vals={histSamples.map(s => s.cpu ?? NaN)} vmax={100} unit="%" color="#89b4fa" />
        <Chart label="RAM" vals={histSamples.map(s => s.memPct ?? NaN)} vmax={100} unit="%" color="#a6e3a1" />
        {#each selectedInst.sys?.gpus ?? [] as g, gi}
          <Chart label={g.name || `GPU ${gi}`} vals={histSamples.map(s => s.gpuPct?.[gi] ?? NaN)} vmax={100} unit="%" color="#f5c2e7" />
        {/each}
      </div>

      {#if (selectedInst.sys?.top_processes ?? []).length}
        <SectionHead title="Top processes">
          {#snippet trailing()}
            <span class="section-meta">click to pin</span>
          {/snippet}
        </SectionHead>
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
            <Chart label={`${pinned.name} CPU`} vals={pinned.cpu} vmax={100} unit="%" color="#fab387" />
            <Chart label={`${pinned.name} RAM`} vals={pinned.mem} vmax={maxMem} unit="MB" color="#cba6f7" />
          </div>
        {/if}
      {/if}

      {#if (selectedInst.addresses ?? []).length > 0}
        <SectionHead title="Addresses" />
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

      <SectionHead title="Update">
        {#snippet trailing()}
          <button
            class="ctrl-btn"
            style="font-size:11px; padding:2px 8px;"
            onclick={probeUpdateState}
            disabled={drawerVersionsLoading || updatePending}
            title="Re-probe this peer's update state"
          >{drawerVersionsLoading ? 'Probing…' : 'Refresh'}</button>
        {/snippet}
      </SectionHead>
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
  /* drawer ident block (still in page until HostDrawer is extracted) */
  .ident {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .hostname {
    font-weight: var(--weight-semibold);
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
  .section-meta {
    margin-left: var(--space-2);
    opacity: 0.55;
    font-weight: 400;
    font-size: 11px;
    text-transform: none;
    letter-spacing: 0;
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
