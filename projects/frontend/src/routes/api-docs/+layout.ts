import type { LayoutLoad } from './$types';
import type { SpecMeta } from '$lib/api/types';
import { listSpecs } from '$lib/api/client';

export const ssr = false;

export const load: LayoutLoad = async () => {
  const specs: SpecMeta[] = await listSpecs().catch(() => [] as SpecMeta[]);
  return { specs };
};
