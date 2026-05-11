import type { PageLoad } from './$types';
import type { TreeNode } from '$lib/api/types';
import { getTree } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  try {
    const tree = await getTree({});
    return {
      tree: (tree != null && typeof tree === 'object' ? tree : {}) as Record<string, TreeNode[]>,
    };
  } catch {
    return { tree: {} };
  }
};
