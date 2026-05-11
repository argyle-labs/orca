import type { PageLoad } from './$types';
import type { SpecMeta } from '$lib/api/types';
import { redirect } from '@sveltejs/kit';

export const ssr = false;

export const load: PageLoad = async ({ parent }) => {
  const { specs } = await parent();
  const first = (specs as SpecMeta[]).find(s => s?.files?.full != null) ?? specs[0];
  if (first) throw redirect(302, `/api-docs/${(first as SpecMeta).repo}`);
  return {};
};
