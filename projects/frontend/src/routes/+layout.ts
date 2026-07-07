// Layout-level pre-fetch. Runs before any child `+page.ts` load() so child
// routes can `await parent()` to read the pod instance roster instead of
// re-calling pod.instances. SvelteKit awaits the entire load chain before
// transitioning, so anything fetched here is already on the page at first
// paint.
//
// Named-import + `unwrap` (not `callTool`) so Rolldown tree-shakes the
// rest of `sdk.gen` out of the layout's chunk. See [[runTool.ts]] header.

import type { LayoutLoad } from './$types';
import { podInstances } from '$lib/client/sdk.gen';
import { unwrap } from '$lib/stores/runTool';
import type { PodInstancesResponse } from '$lib/client/types.gen';

export const load: LayoutLoad = async () => {
  let podRoster: PodInstancesResponse;
  try {
    podRoster = await unwrap(podInstances({ body: {} }));
  } catch {
    // Signed-out or dispatcher rejection — child loads still render.
    podRoster = { members: [], candidates: [], stale: [], inbound_offers: [] };
  }
  return { podRoster };
};

// adapter-static SPA — no server runtime, no prerender.
export const ssr = false;
export const prerender = false;
