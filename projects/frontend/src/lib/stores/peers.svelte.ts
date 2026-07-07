import { systemDetail, podInstances, systemUpdate, podJoin, podForget } from '$lib/client/sdk.gen';
import { unwrap, peerHeader } from '$lib/stores/runTool';
import { notifications } from '$lib/stores/notifications';
import { createPoller } from '$lib/utils/polling';
import type {
  PodInstance,
  PodInboundOffer,
  PodCandidate,
  PodStaleRow,
  VersionEntry,
} from '$lib/client/types.gen';

// Polling cadences. List poll is fast (5s) — fetches pod.instances members.
// Probe poll is slower (60s) — fans out system.update per peer across the
// mesh, which is expensive.
const POLL_MS = 5000;
const PROBE_MS = 60000;

// Frontend-only ephemeral overlay merged onto every PodInstance at read
// time. `actionLockUntil` is the window during which we preserve the
// fields a just-completed mutation authoritatively changed (the next
// pod.instances poll lags behind the peer's true state by one mesh sync).
// `availableVersions` is filled by the per-peer system.update fan-out poll
// and consumed by HostUpdatePanel without forcing a fresh probe.
type Overlay = {
  actionLockUntil?: number;
  availableVersions?: VersionEntry[];
  // Mutation-window field overrides (only present while actionLockUntil > now).
  version?: string | null;
  channel?: string | null;
  pinnedTo?: string | null;
  updateAvailable?: boolean;
  updateLatest?: string | null;
};

function originForLocal(): string {
  if (typeof window === 'undefined') return '';
  return window.location.origin;
}

class PeersStore {
  instances = $state<PodInstance[]>([]);
  inboundOffers = $state<PodInboundOffer[]>([]);
  candidates = $state<PodCandidate[]>([]);
  staleRows = $state<PodStaleRow[]>([]);
  joiningFp = $state<string | null>(null);
  forgettingId = $state<string | null>(null);

  // Ephemeral, frontend-only overlay (lock windows + probed available
  // versions). Keyed by `PodInstance.id`.
  private overlay = new Map<string, Overlay>();

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

  /// Public read for components needing the overlay entry for an instance.
  /// Returns a frozen snapshot — components must call mutation helpers
  /// (`setActionLock`, `setAvailableVersions`, ...) to write back.
  getOverlay(id: string): Readonly<Overlay> {
    return this.overlay.get(id) ?? {};
  }

  setAvailableVersions(id: string, versions: VersionEntry[]) {
    const cur = this.overlay.get(id) ?? {};
    this.overlay.set(id, { ...cur, availableVersions: versions });
  }

  setActionLock(id: string, untilMs: number) {
    const cur = this.overlay.get(id) ?? {};
    this.overlay.set(id, { ...cur, actionLockUntil: untilMs });
  }

  /// Patch a row in place AND record an overlay so the next pod.instances
  /// poll doesn't clobber the values we just mutated. Used by
  /// HostUpdatePanel after `system.update`.
  applyMutation(
    id: string,
    patch: {
      version?: string | null;
      channel?: string | null;
      pinnedTo?: string | null;
      updateAvailable?: boolean;
      updateLatest?: string | null;
    },
    lockMs = 15000,
  ) {
    const idx = this.instances.findIndex(i => i.id === id);
    if (idx >= 0) {
      const row = this.instances[idx];
      if (patch.version !== undefined) row.version = patch.version ?? null;
      if (patch.channel !== undefined) row.channel = patch.channel ?? null;
      if (patch.pinnedTo !== undefined) row.pinned_to = patch.pinnedTo ?? null;
      if (patch.updateAvailable !== undefined) row.update_available = patch.updateAvailable;
      if (patch.updateLatest !== undefined) row.update_latest = patch.updateLatest ?? null;
    }
    const cur = this.overlay.get(id) ?? {};
    this.overlay.set(id, {
      ...cur,
      actionLockUntil: Date.now() + lockMs,
      version: patch.version ?? cur.version,
      channel: patch.channel ?? cur.channel,
      pinnedTo: patch.pinnedTo ?? cur.pinnedTo,
      updateAvailable: patch.updateAvailable ?? cur.updateAvailable,
      updateLatest: patch.updateLatest ?? cur.updateLatest,
    });
  }

  seed(data: {
    instances: PodInstance[];
    candidates: PodCandidate[];
    stale: PodStaleRow[];
    inboundOffers: PodInboundOffer[];
  }) {
    this.assign(data.instances, data.candidates, data.stale, data.inboundOffers);
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

  private assign(
    members: PodInstance[],
    candidates: PodCandidate[],
    stale: PodStaleRow[],
    inboundOffers: PodInboundOffer[],
  ) {
    // Re-stamp the local row's origin to window.location.origin — the
    // daemon emits "" on purpose since it can't know how the browser
    // reached it.
    const localIdx = members.findIndex(i => i.role === 'local');
    if (localIdx >= 0) members[localIdx].origin = originForLocal();

    // Re-apply any active mutation-lock overrides so a fresh poll doesn't
    // briefly snap fields back to the peer's stale value.
    const now = Date.now();
    for (const inst of members) {
      const ov = this.overlay.get(inst.id);
      if (!ov || !ov.actionLockUntil || now >= ov.actionLockUntil) continue;
      if (ov.version !== undefined) inst.version = ov.version ?? null;
      if (ov.channel !== undefined) inst.channel = ov.channel ?? null;
      if (ov.pinnedTo !== undefined) inst.pinned_to = ov.pinnedTo ?? null;
      if (ov.updateAvailable !== undefined) inst.update_available = ov.updateAvailable;
      if (ov.updateLatest !== undefined) inst.update_latest = ov.updateLatest ?? null;
    }

    this.instances = members;
    this.candidates = candidates;
    this.staleRows = stale;
    this.inboundOffers = inboundOffers;
  }

  async refreshLocal(inst: PodInstance) {
    try {
      const [healthRes, detail] = await Promise.all([
        fetch('/api/health', { credentials: 'include' }).catch(() => null),
        unwrap(systemDetail({ body: {} })),
      ]);
      inst.health = healthRes && healthRes.ok ? 'up' : 'down';
      inst.version = detail.version ?? null;
      inst.target = detail.target ?? null;
      inst.mode = detail.mode ?? null;
      inst.channel = detail.channel ?? null;
      inst.pinned_to = detail.pinned_to ?? null;
      inst.system = detail.system ?? null;
      inst.error = null;
    } catch (e) {
      inst.health = 'down';
      inst.error = e instanceof Error ? e.message : String(e);
    } finally {
      inst.last_checked = Date.now();
    }
  }

  async refreshPodPeers() {
    try {
      const r = await unwrap(podInstances({ body: {} }));
      this.assign(r.members ?? [], r.candidates ?? [], r.stale ?? [], r.inbound_offers ?? []);
      this.fireUpdateNotifications();
    } catch (e) {
      console.warn('pod.instances failed:', e);
    }
  }

  async probeAll() {
    const snapshot = this.instances.filter(i => i.health !== 'down');
    await Promise.all(
      snapshot.map(async inst => {
        const peer = inst.role === 'system' ? inst.peer_id : null;
        try {
          const r = await unwrap(systemUpdate({ body: {}, headers: peerHeader(peer) }));
          const target = this.instances.find(i => i.id === inst.id);
          if (!target) return;
          const ov = this.overlay.get(target.id);
          if (ov?.actionLockUntil && Date.now() < ov.actionLockUntil) return;
          if (r.current_version) target.version = r.current_version;
          target.channel = r.channel ?? target.channel;
          target.pinned_to = r.pinned_to ?? null;
          if (r.latest) {
            target.update_latest = r.latest;
            target.update_available = r.update_available === true;
          } else {
            target.update_available = false;
          }
          this.setAvailableVersions(target.id, r.available_versions ?? []);
          target.last_checked = Date.now();
        } catch (e) {
          console.debug(`system.update probe failed for ${inst.label}:`, e);
        }
      }),
    );
    this.fireUpdateNotifications();
  }

  private fireUpdateNotifications() {
    for (const inst of this.instances) {
      if (inst.update_available && !this.notifiedUpdates.has(inst.id)) {
        this.notifiedUpdates.add(inst.id);
        const name = inst.system?.hostname ?? inst.label;
        const ver = inst.update_latest ? ` (${inst.update_latest})` : '';
        notifications.info(`${name} has an update available${ver}`);
      }
    }
  }

  async joinCandidate(c: PodCandidate) {
    if (this.joiningFp) return;
    this.joiningFp = c.pubkey_fp;
    try {
      await unwrap(podJoin({ body: { action: 'invite', addr: c.addr, port: c.port } }));
      notifications.info(`Invite sent to ${c.hostname || c.addr}`);
      await this.refreshPodPeers();
    } catch (e) {
      notifications.error(`Join failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      this.joiningFp = null;
    }
  }

  async forgetPeer(s: PodStaleRow) {
    if (this.forgettingId) return;
    this.forgettingId = s.peer_id;
    try {
      const r = await unwrap(podForget({ body: { peer_id: s.peer_id } }));
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
