// Layout-level pre-fetch. Runs before any child `+page.ts` load() so child
// routes can `await parent()` to read `peers` instead of re-calling
// `pod.list`. SvelteKit awaits the entire load chain before transitioning,
// so anything fetched here is already on the page at first paint.
//
// Named-import + `unwrap` (not `callTool`) so Rolldown tree-shakes the
// rest of `sdk.gen` out of the layout's chunk. See [[runTool.ts]] header.

import type { LayoutLoad } from './$types';
import { podList } from '$lib/client/sdk.gen';
import { unwrap } from '$lib/stores/runTool';
import type { PodListResponse } from '$lib/client/types.gen';

export const load: LayoutLoad = async () => {
  let peers: PodListResponse;
  try {
    peers = await unwrap(podList({ body: {} }));
  } catch {
    // Signed-out or dispatcher rejection — child loads still render.
    peers = { members: [] };
  }
  return { peers };
};

// adapter-static SPA — no server runtime, no prerender.
export const ssr = false;
export const prerender = false;
