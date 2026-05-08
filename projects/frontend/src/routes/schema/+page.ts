import type { PageLoad } from './$types';
import { getSchema, listSchemaDatabases } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  const [schema, databases] = await Promise.allSettled([getSchema(), listSchemaDatabases()]);
  return {
    schema: schema.status === 'fulfilled' ? schema.value : null,
    databases: databases.status === 'fulfilled' ? databases.value : [],
  };
};
