// Pre-fetch everything the systems list reads on FIRST PAINT:
//   - peers (from +layout.ts via await parent())
//   - local system detail
//   - retention-days config
//   - per-peer system.update {} probe fan-out
// SvelteKit awaits load() before swapping the page in, so the table is
// fully populated at first render — onMount only registers polling timers.
//
// HARD RULE: every value returned from load() is an SDK-generated type
// from `$lib/client/types.gen` — no hand-rolled shapes here. The page
// component synthesizes its `Instance[]` view-model from these raw
// responses on first render.

import type { PageLoad } from './$types';
import { callTool } from '$lib/stores/runTool';
import type {
  SystemDetailResponse,
  SystemUpdateResponse,
  ConfigGetResponse,
  PodPeerDto,
} from '$lib/client/types.gen';

export const load: PageLoad = async ({ parent, fetch }) => {
  const { peers } = await parent();
  const members = peers.members ?? [];

  // Joined + remote peers — same filter the page applies when building
  // `instances[]`. `PodMember` is a union; narrow by the `peer_id`
  // discriminator unique to `PodPeerDto`.
  const remotePeers = members.filter(
    (m): m is PodPeerDto =>
      'peer_id' in m && (m as PodPeerDto).status === 'active' && !(m as PodPeerDto).local,
  );

  const localHealthPromise = fetch('/api/health', { credentials: 'include' })
    .then(r => r.ok)
    .catch(() => false);

  const localDetailPromise = callTool<SystemDetailResponse>('systemDetail', {}).catch(() => null);
  const retentionPromise = callTool<ConfigGetResponse>('configGet', {
    noun: 'host_status',
    name: 'retention_days',
  }).catch(() => null);

  // One system.update {} probe per instance (local + each remote). The
  // page's probeAllInstances does the same fan-out on a 60 s interval;
  // doing it here makes the FIRST render carry probe data too.
  const probePromise = (async () => {
    const probes: Record<string, SystemUpdateResponse> = {};
    const tasks: Promise<void>[] = [];
    tasks.push(
      (async () => {
        try {
          probes['local'] = await callTool<SystemUpdateResponse>('systemUpdate', {});
        } catch {
          // single-probe failure shouldn't block first paint
        }
      })(),
    );
    for (const p of remotePeers) {
      tasks.push(
        (async () => {
          try {
            probes[p.peer_id] = await callTool<SystemUpdateResponse>(
              'systemUpdate',
              {},
              { peer: p.peer_id },
            );
          } catch {
            // ignore — one peer probe failure shouldn't block render
          }
        })(),
      );
    }
    await Promise.all(tasks);
    return probes;
  })();

  const [localHealthy, localDetail, retention, probes] = await Promise.all([
    localHealthPromise,
    localDetailPromise,
    retentionPromise,
    probePromise,
  ]);

  return {
    localHealthy,
    localDetail,
    retention,
    probes,
  };
};

export const ssr = false;
export const prerender = false;
