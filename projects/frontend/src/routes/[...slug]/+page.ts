import type { PageLoad } from './$types';
import { getDoc } from '$lib/api/client';
import { error } from '@sveltejs/kit';

export const ssr = false;

export const load: PageLoad = async ({ params }) => {
  const slug = params.slug ?? '';
  const parts = slug.split('/').filter(Boolean);
  const root = parts[0] ?? 'orca';
  const path = parts.slice(1).join('/');
  if (!path) return { content: '', root, path: '' };
  try {
    const raw = await getDoc({ root, path });
    return { content: String(raw ?? ''), root, path };
  } catch (e: any) {
    throw error(404, e?.message ?? 'Not found');
  }
};
