import { proxmoxClusterList } from '$lib/client/sdk.gen';
import { unwrap } from '$lib/stores/runTool';
import { createPoller } from '$lib/utils/polling';
import type { Instance, ClusterSummary } from '$lib/types/instance';

const POLL_MS = 60000;

class ProxmoxClustersStore {
  summaries = $state<Map<string, ClusterSummary>>(new Map());
  // Internal: cluster name lookup indexes built from cluster_list response.
  // Exposed for the resolver below.
  byIp = $state<Map<string, string>>(new Map());
  byHost = $state<Map<string, string>>(new Map());

  private poller = createPoller({ intervalMs: POLL_MS, fn: () => this.refresh() });
  private started = false;

  start() {
    if (this.started) return;
    this.started = true;
    this.poller.start();
  }

  stop() {
    if (!this.started) return;
    this.started = false;
    this.poller.stop();
  }

  async refresh() {
    try {
      const list = await unwrap(proxmoxClusterList({ body: {} }));
      const byIp = new Map<string, string>();
      const byHost = new Map<string, string>();
      const summaries = new Map<string, ClusterSummary>();
      for (const entry of list ?? []) {
        const cname = entry.status.name;
        if (!cname) continue;
        const total = entry.status.nodes.length;
        const online = entry.status.nodes.filter(n => n.online === true).length;
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
      this.byIp = byIp;
      this.byHost = byHost;
      this.summaries = summaries;
    } catch (e) {
      console.warn('proxmox.cluster_list failed:', e);
    }
  }
}

export const proxmoxClusters = new ProxmoxClustersStore();

// Pure resolver: build peerId → cluster name map (null = ungrouped) from
// the store's lookup indexes and the current instance list.
export function resolvePeerClusters(
  instances: Instance[],
  byIp: Map<string, string>,
  byHost: Map<string, string>,
): Map<string, string | null> {
  const out = new Map<string, string | null>();
  for (const inst of instances) {
    let matched: string | null = null;
    for (const a of inst.addresses ?? []) {
      const hit = byIp.get(a.value);
      if (hit) {
        matched = hit;
        break;
      }
    }
    if (!matched && inst.sys?.primary_ipv4) {
      matched = byIp.get(inst.sys.primary_ipv4) ?? null;
    }
    if (!matched) {
      const host = (inst.sys?.hostname ?? inst.label ?? '').toLowerCase();
      if (host) matched = byHost.get(host) ?? null;
    }
    out.set(inst.peerId, matched);
  }
  return out;
}
