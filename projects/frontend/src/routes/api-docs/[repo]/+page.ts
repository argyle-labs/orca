import type { PageLoad } from './$types';
import type { SpecMeta } from '$lib/api/types';

export const ssr = false;

export const load: PageLoad = async ({ params, parent }) => {
  const { specs } = await parent();
  const spec = (specs as SpecMeta[]).find(s => s.repo === params.repo) ?? null;
  return { repo: params.repo, spec };
};
