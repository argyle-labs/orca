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
// HARD RULE: every value returned from load() is an SDK-generated type
// from `$lib/client/types.gen` — no hand-rolled shapes here.

import type { PageLoad } from './$types';
import { callTool } from '$lib/stores/runTool';
import type { SystemDetailResponse, ConfigGetResponse } from '$lib/client/types.gen';

export const load: PageLoad = async ({ parent, fetch }) => {
  await parent();

  const localHealthPromise = fetch('/api/health', { credentials: 'include' })
    .then(r => r.ok)
    .catch(() => false);

  const localDetailPromise = callTool<SystemDetailResponse>('systemDetail', {}).catch(() => null);
  const retentionPromise = callTool<ConfigGetResponse>('configGet', {
    noun: 'host_status',
    name: 'retention_days',
  }).catch(() => null);

  const [localHealthy, localDetail, retention] = await Promise.all([
    localHealthPromise,
    localDetailPromise,
    retentionPromise,
  ]);

  return {
    localHealthy,
    localDetail,
    retention,
    // Empty seed — onMount fires probeAllInstances() immediately, and
    // each peer row updates as its probe returns. Until then the page
    // renders with version/channel/pinned-to fields from PodPeerDto.
    probes: {},
  };
};

export const ssr = false;
export const prerender = false;
