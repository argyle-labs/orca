import type { PageLoad } from './$types';

export const ssr = false;

export const load: PageLoad = async ({ params, parent }) => {
  const { specs } = await parent();
  const spec = (specs as any[]).find((s: any) => s.repo === params.repo) ?? null;
  return { repo: params.repo, spec };
};
