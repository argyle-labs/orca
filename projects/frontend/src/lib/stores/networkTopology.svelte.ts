import { networkTopologyView } from '$lib/client/sdk.gen';
import { unwrap } from '$lib/stores/runTool';
import { createPoller } from '$lib/utils/polling';
import type { TopologyNode, TopologyEdge } from '$lib/client/types.gen';

const POLL_MS = 30000;

// Server-side network topology graph. `network.topology_view` returns a flat
// node + edge list suitable for force-directed canvas rendering. Nodes are
// peers (plus compound cluster parents); edges are parent-inference
// relationships (MAC claim or explicit `parent_peer_id` override).
class NetworkTopologyStore {
  nodes = $state<TopologyNode[]>([]);
  edges = $state<TopologyEdge[]>([]);

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
      const out = await unwrap(networkTopologyView({ body: {} }));
      this.nodes = out.nodes;
      this.edges = out.edges;
    } catch (e) {
      console.warn('network.topology_view failed:', e);
    }
  }
}

export const networkTopologyStore = new NetworkTopologyStore();
