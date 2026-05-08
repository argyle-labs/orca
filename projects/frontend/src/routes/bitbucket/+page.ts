import type { PageLoad } from './$types';
import { listBitbucketRepos, getPluginData } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  const configResult = await getPluginData({ id: 'rebuy', key: 'bitbucket_config' }).catch(
    () => null,
  );
  const config = configResult?.value ? JSON.parse(configResult.value) : null;

  try {
    const repos = await listBitbucketRepos();
    return { repos, config };
  } catch {
    return { repos: [], config };
  }
};
