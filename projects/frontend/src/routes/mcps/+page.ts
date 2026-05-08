import type { PageLoad } from './$types';

export const ssr = false;

export const load: PageLoad = async ({ fetch }) => {
  try {
    const res = await fetch('/api/mcp/mappings');
    const mappings = await res.json();
    return { mappings };
  } catch {
    return { mappings: [] };
  }
};
