// Pre-fetch the peer + its system.update probe so the detail page is fully
// populated at first paint (no data snap-in during navigation from /).
// The instance roster comes from +layout.ts via await parent() — no
// duplicate pod.instances call.
//
// HARD RULE: every value returned from load() is an SDK-generated type.

import type { PageLoad } from './$types';
import { callTool } from '$lib/stores/runTool';
import type { SystemUpdateResponse, PodInstance } from '$lib/client/types.gen';

export const load: PageLoad = async ({ parent, params }) => {
  const { podRoster } = await parent();
  const members = podRoster.members ?? [];
  // `id === 'local'` is the synthetic id the list page uses for "this host"
  // — the pod.instances tool emits role==='local' for that row.
  const found: PodInstance | null =
    params.id === 'local'
      ? (members.find(m => m.role === 'local') ?? null)
      : (members.find(m => m.peer_id === params.id) ?? null);

  let probe: SystemUpdateResponse | null = null;
  if (found) {
    const target = found.role === 'local' ? null : found.peer_id;
    try {
      probe = await callTool<SystemUpdateResponse>('systemUpdate', {}, { peer: target });
    } catch {
      // probe failure shouldn't block first paint
    }
  }

  return { peer: found, probe };
};

export const ssr = false;
export const prerender = false;
