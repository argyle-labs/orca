import type { PageLoad } from './$types';
import { getTree } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  try {
    const tree = await getTree({});
    return { tree: (tree ?? {}) as Record<string, any[]> };
  } catch {
    return { tree: {} };
  }
};
