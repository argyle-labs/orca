import type { PageLoad } from './$types';
import { runMcpTool } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  const [problemsResult, progressResult] = await Promise.allSettled([
    runMcpTool({
      body: {
        server: 'leetcode',
        name: 'leetcode_list_problems',
        arguments: { limit: 800, lang: 'ts' },
      },
    }),
    runMcpTool({
      body: { server: 'leetcode', name: 'leetcode_get_progress', arguments: { lang: 'ts' } },
    }),
  ]);

  return {
    problemsText:
      problemsResult.status === 'fulfilled' ? (problemsResult.value?.content?.[0]?.text ?? '') : '',
    progressText:
      progressResult.status === 'fulfilled' ? (progressResult.value?.content?.[0]?.text ?? '') : '',
  };
};
