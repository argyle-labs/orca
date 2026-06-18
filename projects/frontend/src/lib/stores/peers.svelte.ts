import { callTool } from '$lib/stores/runTool';
import { notifications } from '$lib/stores/notifications';
import { createPoller } from '$lib/utils/polling';
import {
  seedInstancesFromLoad,
  seedInboundOffersFromLoad,
  type InstanceSeedInput,
} from '$lib/utils/instances';
import type {
  PodMember,
  PodPeerDto,
  PodPendingOfferDto,
  PodDiscoveryRowDto,
  PodForgetResponse,
  SystemInfoReport,
} from '$lib/client/types.gen';
import type {
  Instance,
  InboundOffer,
  Candidate,
  StaleRow,
  SystemUpdateResp,
} from '$lib/types/instance';

// Polling cadences. List poll is fast (5s) — fetches pod.list members.
// Probe poll is slower (60s) — fans out system.update per peer across the
// mesh, which is expensive.
const POLL_MS = 5000;
const PROBE_MS = 60000;

function isPodPeerDto(m: PodMember): m is PodPeerDto {
  return 'peer_id' in m && (m as PodPeerDto).status !== undefined;
}

function isPodPendingOfferDto(m: PodMember): m is PodPendingOfferDto {
  return (m as { state?: string }).state === 'handshaking';
}

function isPodDiscoveryRowDto(m: PodMember): m is PodDiscoveryRowDto {
  return (m as { state?: string }).state === 'discovered';
}

class PeersStore {
  instances = $state<Instance[]>([]);
  inboundOffers = $state<InboundOffer[]>([]);
  candidates = $state<Candidate[]>([]);
  staleRows = $state<StaleRow[]>([]);
  joiningFp = $state<string | null>(null);
  forgettingId = $state<string | null>(null);

  private listPoller = createPoller({
    intervalMs: POLL_MS,
    immediate: false,
    fn: () => this.tickList(),
  });
  private probePoller = createPoller({
    intervalMs: PROBE_MS,
    fn: () => this.probeAll(),
  });
  private notifiedUpdates = new Set<string>();
  private started = false;

  seed(data: InstanceSeedInput) {
    this.instances = seedInstancesFromLoad(data);
    this.inboundOffers = seedInboundOffersFromLoad(data);
  }

  start() {
    if (this.started) return;
    this.started = true;
    this.listPoller.start();
    this.probePoller.start();
  }

  stop() {
    if (!this.started) return;
    this.started = false;
    this.listPoller.stop();
    this.probePoller.stop();
  }

  private tickList() {
    const loc = this.instances.find(i => i.role === 'local');
    if (loc) void this.refreshLocal(loc);
    void this.refreshPodPeers();
  }

  async refreshLocal(inst: Instance) {
    try {
      const [healthRes, detail] = await Promise.all([
        fetch('/api/health', { credentials: 'include' }).catch(() => null),
        callTool<{
          version: string;
          target: string;
          frontend: string;
          mode?: string;
          channel?: string;
          pinned_to?: string;
          system?: SystemInfoReport | null;
        }>('systemDetail', {}),
      ]);
      inst.health = healthRes && healthRes.ok ? 'up' : 'down';
      inst.version = detail.version ?? null;
      inst.target = detail.target ?? null;
      inst.mode = detail.mode ?? null;
      inst.channel = detail.channel ?? null;
      inst.pinnedTo = detail.pinned_to ?? null;
      inst.sys = detail.system ?? null;
      inst.error = null;
    } catch (e) {
      inst.health = 'down';
      inst.error = e instanceof Error ? e.message : String(e);
    } finally {
      inst.lastChecked = Date.now();
    }
  }

  async refreshPodPeers() {
    try {
      const listResult = await callTool<{ members: PodMember[] }>('podList', {});
      const members = listResult?.members ?? [];

      // Inbound offers piggy-back on this single pod.list payload.
      const nowSec = Math.floor(Date.now() / 1000);
      this.inboundOffers = members.filter(isPodPendingOfferDto).filter(r => r.expires_at > nowSec);

      const joined = members.filter(isPodPeerDto);
      const peersResult = joined.filter(p => p.status === 'active');
      const selfPeer = joined.find(p => p.local);
      const ownHostname = (selfPeer?.hostname ?? '').toLowerCase();
      const activePeerIds = new Set(peersResult.map(p => p.peer_id));

      const staleFromJoined: StaleRow[] = joined
        .filter(p => !p.local && p.status !== 'active')
        .map(p => ({
          peer_id: p.peer_id,
          hostname: p.hostname ?? p.peer_id,
          addr: p.addr ?? '',
          port: p.port ?? 0,
          reason: 'departed',
          last_seen_at: null,
        }));

      const discovered = members
        .filter(isPodDiscoveryRowDto)
        .filter(d => !(d.peer_id && activePeerIds.has(d.peer_id)));

      const nextCandidates: Candidate[] = [];
      const staleFromDiscovery: StaleRow[] = [];
      for (const d of discovered) {
        const isSelfEcho = (d.hostname ?? '').toLowerCase() === ownHostname && !!ownHostname;
        const unclaimed = d.discovery_state === 'unclaimed';
        if (unclaimed && !isSelfEcho) {
          nextCandidates.push({
            pubkey_fp: d.pubkey_fp,
            peer_id: d.peer_id ?? null,
            hostname: d.hostname,
            addr: d.addr,
            port: d.port,
            can_invite: d.can_invite,
          });
        } else if (d.peer_id) {
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
      this.candidates = nextCandidates;
      this.staleRows = [...staleFromJoined, ...staleFromDiscovery];

      const sysById = new Map<string, SystemInfoReport | null>();
      for (const p of joined) sysById.set(p.peer_id, p.system ?? null);

      const local = this.instances.find(i => i.role === 'local');
      const now = Date.now();
      const existingById = new Map(this.instances.map(i => [i.id, i] as const));
      const podRows: Instance[] = peersResult
        .filter(p => !p.local)
        .map(p => {
          const id = `system:${p.peer_id}`;
          const prev = existingById.get(id);
          const storedSys = sysById.get(p.peer_id) ?? null;
          const sys = p.system ?? storedSys;
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
            addresses: (p.addresses ?? []).map(a => ({ kind: a.kind, value: a.value })),
            sys,
            actionLockUntil: prev?.actionLockUntil,
          };
        });
      this.instances = local ? [local, ...podRows] : podRows;
      this.fireUpdateNotifications();
    } catch (e) {
      console.warn('pod.list failed:', e);
    }
  }

  async probeAll() {
    const snapshot = this.instances.filter(i => i.health !== 'down');
    await Promise.all(
      snapshot.map(async inst => {
        const peer = inst.role === 'system' ? inst.peerId : null;
        try {
          const r = await callTool<SystemUpdateResp>('systemUpdate', {}, { peer });
          const target = this.instances.find(i => i.id === inst.id);
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
        } catch (e) {
          console.debug(`system.update probe failed for ${inst.label}:`, e);
        }
      }),
    );
    this.fireUpdateNotifications();
  }

  private fireUpdateNotifications() {
    for (const inst of this.instances) {
      if (inst.updateAvailable && !this.notifiedUpdates.has(inst.id)) {
        this.notifiedUpdates.add(inst.id);
        const name = inst.sys?.hostname ?? inst.label;
        const ver = inst.updateLatest ? ` (${inst.updateLatest})` : '';
        notifications.info(`${name} has an update available${ver}`);
      }
    }
  }

  async joinCandidate(c: Candidate) {
    if (this.joiningFp) return;
    this.joiningFp = c.pubkey_fp;
    try {
      await callTool('podJoin', { action: 'invite', addr: c.addr, port: c.port });
      notifications.info(`Invite sent to ${c.hostname || c.addr}`);
      await this.refreshPodPeers();
    } catch (e) {
      notifications.error(`Join failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      this.joiningFp = null;
    }
  }

  async forgetPeer(s: StaleRow) {
    if (this.forgettingId) return;
    this.forgettingId = s.peer_id;
    try {
      const r = await callTool<PodForgetResponse>('podForget', { peer_id: s.peer_id });
      notifications.info(
        `Forgot ${s.hostname || s.peer_id} (${r.rows_removed} rows, ${r.notified.length} peers notified)`,
      );
      await this.refreshPodPeers();
    } catch (e) {
      notifications.error(`Forget failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      this.forgettingId = null;
    }
  }
}

export const peers = new PeersStore();
