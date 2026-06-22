// First paint pulls the fully-shaped pod roster from the layout (which
// already awaited pod.instances) — no extra fetches here. The store seeds
// from `parent().podRoster` and the page-level probe poll then refines
// per-peer fields as `system.update` returns.

import type { PageLoad } from './$types';

export const load: PageLoad = async ({ parent }) => {
  const { podRoster } = await parent();
  return {
    instances: podRoster.members,
    candidates: podRoster.candidates,
    stale: podRoster.stale,
    inboundOffers: podRoster.inbound_offers,
  };
};

export const ssr = false;
export const prerender = false;
