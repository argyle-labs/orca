import type { PageLoad } from './$types';
import { redirect } from '@sveltejs/kit';

export const ssr = false;

export const load: PageLoad = async ({ parent }) => {
  const { specs } = await parent();
  const first = specs.find((s: any) => s?.files?.full != null && s?.files?.full !== false)
             ?? specs[0];
  if (first) throw redirect(302, `/api-docs/${first.repo}`);
  return {};
};
