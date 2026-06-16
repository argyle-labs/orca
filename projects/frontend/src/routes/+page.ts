// FIRST PAINT MUST NOT BLOCK ON MESH ROUND-TRIPS.
//
// `load()` returns ONLY values cheap enough to compute on the local
// daemon in <50ms total: peers (already in +layout), local health,
// local system.detail, retention config. The per-peer `system.update`
// fan-out is NOT awaited here — it would pin first paint to the
// slowest peer across the mesh (8 hosts × ~500ms = 4s blank screen).
//
// The page seeds version/channel/pinned-to from `PodPeerDto` fields
// (already populated by pod.list — see roster-sync pubkey_fp work) on
// first render, then onMount kicks off `probeAllInstances()`
// immediately so any drift from those cached values is corrected
// within one mesh RTT per peer — and each row updates the moment its
// own probe returns, independently of the slowest peer.
//
// Named-import + `unwrap` (not `callTool`) — only the two functions
// imported here are pulled into the page's chunk. See [[runTool.ts]] header.

import type { PageLoad } from './$types';
import { systemDetail, configGet } from '$lib/client/sdk.gen';
import { unwrap } from '$lib/stores/runTool';
import type {
  SystemDetailResponse,
  ConfigGetResponse,
  SystemUpdateResponse,
} from '$lib/client/types.gen';

export const load: PageLoad = async ({ parent, fetch }) => {
  await parent();

  const localHealthPromise = fetch('/api/health', { credentials: 'include' })
    .then(r => r.ok)
    .catch(() => false);

  const localDetailPromise: Promise<SystemDetailResponse | null> = unwrap(
    systemDetail({ body: {} }),
  ).catch(() => null);
  const retentionPromise: Promise<ConfigGetResponse | null> = unwrap(
    configGet({ body: { noun: 'host_status', name: 'retention_days' } }),
  ).catch(() => null);

  const [localHealthy, localDetail, retention] = await Promise.all([
    localHealthPromise,
    localDetailPromise,
    retentionPromise,
  ]);

  // Empty probes seed — onMount fires probeAllInstances() immediately,
  // and each peer row updates as its probe returns. Until then the page
  // renders with version/channel/pinned-to fields from PodPeerDto.
  const probes: Record<string, SystemUpdateResponse> = {};

  return {
    localHealthy,
    localDetail,
    retention,
    probes,
  };
};

export const ssr = false;
export const prerender = false;
