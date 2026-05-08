import type { LayoutLoad } from './$types';
import { listSpecs } from '$lib/api/client';

export const ssr = false;

export const load: LayoutLoad = async () => {
  const specs = await listSpecs().catch(() => [] as any[]);
  return { specs: specs as any[] };
};
