// Pre-fetch the peer + its system.update probe so the detail page is fully
// populated at first paint (no data snap-in during navigation from /).
// peers come from +layout.ts via await parent() — no duplicate pod.list.
//
// HARD RULE: every value returned from load() is an SDK-generated type.

import type { PageLoad } from './$types';
import { callTool } from '$lib/stores/runTool';
import type { SystemUpdateResponse, PodPeerDto } from '$lib/client/types.gen';

export const load: PageLoad = async ({ parent, params }) => {
  const { peers } = await parent();
  const members = peers.members ?? [];
  const joined = members.filter(
    (m): m is PodPeerDto => 'peer_id' in m && (m as PodPeerDto).status !== undefined,
  );
  // `id === 'local'` is the synthetic id the list page uses for "this host"
  // before pod.list ever assigns a real peer_id. Match the pod.list member
  // flagged `local: true` so the detail route works for the local card too.
  const found =
    params.id === 'local'
      ? (joined.find(p => p.local) ?? null)
      : (joined.find(p => p.peer_id === params.id) ?? null);

  let probe: SystemUpdateResponse | null = null;
  if (found) {
    const target = found.local ? null : found.peer_id;
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
