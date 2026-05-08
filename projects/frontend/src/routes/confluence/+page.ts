import type { PageLoad } from './$types';
import { getPluginData } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  const configResult = await getPluginData({ id: 'rebuy', key: 'confluence_config' }).catch(
    () => null,
  );
  const config = configResult?.value ? JSON.parse(configResult.value) : null;
  return { config };
};
