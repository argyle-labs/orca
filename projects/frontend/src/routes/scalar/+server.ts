import { ScalarApiReference } from '@scalar/sveltekit';
import type { RequestHandler } from './$types';

export const GET: RequestHandler = ({ url }) => {
  const specUrl = url.searchParams.get('url') ?? '/api/openapi.json';
  return ScalarApiReference({ url: specUrl })();
};
