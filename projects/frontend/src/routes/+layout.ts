// Layout-level pre-fetch. Runs before any child `+page.ts` load() so child
// routes can `await parent()` to read `peers` instead of re-calling
// `pod.list`. SvelteKit awaits the entire load chain before transitioning,
// so anything fetched here is already on the page at first paint — no
// data snap-in.

import type { LayoutLoad } from './$types';
import { callTool } from '$lib/stores/runTool';
import type { PodListResponse } from '$lib/client/types.gen';

export const load: LayoutLoad = async () => {
  // Best-effort: if the call fails (e.g. signed out and the dispatcher
  // rejects), fall back to an empty roster so child loads can still render.
  let peers: PodListResponse;
  try {
    peers = await callTool<PodListResponse>('podList', {});
  } catch {
    peers = { members: [] };
  }
  return { peers };
};

// adapter-static SPA — no server runtime, no prerender.
export const ssr = false;
export const prerender = false;
