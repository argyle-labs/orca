import type { PageLoad } from './$types';
import { listJiraIssues, getPluginData } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  const configResult = await getPluginData({ id: 'rebuy', key: 'jira_config' }).catch(() => null);
  const config = (configResult?.value as { jql?: string; project?: string } | null) ?? null;

  if (!config?.jql) return { issues: [], config: null };

  try {
    const issues = await listJiraIssues({ jql: config.jql, maxResults: 50 });
    return { issues: (issues as any)?.issues ?? [], config };
  } catch {
    return { issues: [], config };
  }
};
