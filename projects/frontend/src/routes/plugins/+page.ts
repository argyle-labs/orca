import type { PageLoad } from './$types';
import { listPlugins } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  const plugins = await listPlugins().catch(() => []);
  return { plugins };
};
