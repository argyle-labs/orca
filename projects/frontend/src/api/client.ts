// ⚠️  AUTO-GENERATED — do not edit. Run `orca gen` to regenerate.
import type * as T from './types';
const BASE = ''; // same-origin — proxied via Vite in dev
async function request<R>(
  method: string,
  path: string,
  opts?: {
    query?: Record<string, string | number | boolean | undefined>;
    body?: unknown;
  },
): Promise<R> {
  const url = new URL(BASE + path, window.location.origin);
  if (opts?.query) {
    for (const [k, v] of Object.entries(opts.query)) {
      if (v !== undefined) url.searchParams.set(k, String(v));
    }
  }
  const res = await fetch(url.toString(), {
    method,
    headers: opts?.body ? { 'Content-Type': 'application/json' } : undefined,
    body: opts?.body ? JSON.stringify(opts.body) : undefined,
  });
  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(err.error ?? `HTTP ${res.status}`);
  }
  const ct = res.headers.get('content-type') ?? '';
  if (ct.includes('application/json')) return res.json();
  return res.text() as unknown as R;
}
/** GET /api/bitbucket/prs?workspace=X&slug=Y
Proxies to the Bitbucket REST API using stored Atlassian credentials. */
export async function listBitbucketPRs(params: {
  workspace: string; // Bitbucket workspace slug
  slug: string; // Repository slug
}): Promise<unknown> {
  const { workspace, slug } = params;

  return request<unknown>('GET', '/api/bitbucket/prs', {
    query: { workspace, slug },
  });
}
/** GET /api/bitbucket/repos
Scans REBUY_ROOT (or ~/code/rebuy) for git dirs with Bitbucket remotes. */
export async function listBitbucketRepos(): Promise<T.RepoInfo[]> {
  return request<T.RepoInfo[]>('GET', '/api/bitbucket/repos', {});
}
export async function searchConfluence(params: {
  cql?: string; // CQL query (default: type = page ORDER BY lastModified DESC)
  limit?: number; // Max results (default: 25)
}): Promise<unknown> {
  const { cql, limit } = params;

  return request<unknown>('GET', '/api/confluence/search', {
    query: { cql, limit },
  });
}
export async function getLibraryDocs(params: {
  q: string; // Library name to look up (npm package, crate, etc.)
  topic?: string; // Specific topic or function to focus on
}): Promise<T.Ctx7Response> {
  const { q, topic } = params;

  return request<T.Ctx7Response>('GET', '/api/ctx7', {
    query: { q, topic },
  });
}
export async function getDoc(params: {
  root: string; // Vault root name (orca/rebuy/docs)
  path: string; // File path relative to root
  format?: string; // Pass `llm` to strip decorative markdown (bold, italic, images, HRs) and collapse whitespace — reduces token usage when the content will be read by a language model
}): Promise<void> {
  const { root, path, format } = params;

  return request<void>('GET', '/api/doc', {
    query: { root, path, format },
  });
}
export async function runDockerAction(params: {
  body: T.DockerActionRequest;
}): Promise<T.DockerActionResponse> {
  return request<T.DockerActionResponse>('POST', '/api/docker/action', {
    body: params.body,
  });
}
export async function getDockerEngine(): Promise<void> {
  return request<void>('GET', '/api/docker/engine', {});
}
export async function startDockerEngine(): Promise<void> {
  return request<void>('POST', '/api/docker/engine/start', {});
}
export async function listDockerRuntimes(): Promise<T.DockerRuntimeInfo[]> {
  return request<T.DockerRuntimeInfo[]>('GET', '/api/docker/runtimes', {});
}
export async function addDockerRuntime(params: {
  body: T.DockerRuntimeAddRequest;
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('POST', '/api/docker/runtimes', {
    body: params.body,
  });
}
export async function removeDockerRuntime(params: {
  name: string; // Runtime name
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('DELETE', `/api/docker/runtimes/${params.name}`, {});
}
export async function getDockerServices(params: {
  path: string; // Absolute path to the Docker Compose project directory
}): Promise<T.DockerServicesResponse> {
  const { path } = params;

  return request<T.DockerServicesResponse>('GET', '/api/docker/services', {
    query: { path },
  });
}
export async function ping(): Promise<void> {
  return request<void>('GET', '/api/health', {});
}
export async function listJiraIssues(params: {
  jql?: string; // JQL query (default: assignee = currentUser() ORDER BY updated DESC)
  maxResults?: number; // Max results to return (default: 50)
}): Promise<unknown> {
  const { jql, maxResults } = params;

  return request<unknown>('GET', '/api/jira/issues', {
    query: { jql, maxResults },
  });
}
export async function getJiraTransitions(params: {
  key: string; // Jira issue key (e.g. PROJ-123)
}): Promise<unknown> {
  return request<unknown>('GET', `/api/jira/issues/${params.key}/transitions`, {});
}
export async function transitionJiraIssue(params: {
  key: string; // Jira issue key (e.g. PROJ-123)
  body: T.TransitionBody;
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('POST', `/api/jira/issues/${params.key}/transitions`, {
    body: params.body,
  });
}
export async function getLearningProgress(): Promise<T.ProgressResponse> {
  return request<T.ProgressResponse>('GET', '/api/learning/progress', {});
}
export async function saveLearningProgress(params: { body: T.ProgressRequest }): Promise<void> {
  return request<void>('POST', '/api/learning/progress', {
    body: params.body,
  });
}
export async function getLogs(params: {
  project: string; // Absolute path to the project directory
  service?: string; // Specific service name (omit for all)
  tail?: number; // Number of log lines to return (default 200)
}): Promise<T.LogsResponse> {
  const { project, service, tail } = params;

  return request<T.LogsResponse>('GET', '/api/logs', {
    query: { project, service, tail },
  });
}
export async function getLogServices(): Promise<T.LogServicesResponse> {
  return request<T.LogServicesResponse>('GET', '/api/logs/services', {});
}
export async function listMcpMappings(params: {
  name?: string; // Filter by MCP server name
}): Promise<T.MappingRow[]> {
  const { name } = params;

  return request<T.MappingRow[]>('GET', '/api/mcp/mappings', {
    query: { name },
  });
}
export async function createMcpMapping(params: { body: T.MapRequest }): Promise<T.OkResponse> {
  return request<T.OkResponse>('POST', '/api/mcp/mappings', {
    body: params.body,
  });
}
export async function deleteMcpMapping(params: {
  orca_tool: string; // Orca tool name to unmap
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('DELETE', `/api/mcp/mappings/${params.orca_tool}`, {});
}
export async function runMcpTool(params: { body: T.McpRunRequest }): Promise<T.McpRunResponse> {
  return request<T.McpRunResponse>('POST', '/api/mcp/run', {
    body: params.body,
  });
}
export async function listMcpServers(): Promise<T.McpServerInfo[]> {
  return request<T.McpServerInfo[]>('GET', '/api/mcp/servers', {});
}
export async function addMcpServer(params: { body: T.McpServerAddRequest }): Promise<T.OkResponse> {
  return request<T.OkResponse>('POST', '/api/mcp/servers', {
    body: params.body,
  });
}
export async function removeMcpServer(params: {
  name: string; // Server name
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('DELETE', `/api/mcp/servers/${params.name}`, {});
}
export async function getMcpTools(): Promise<T.McpToolInfo[]> {
  return request<T.McpToolInfo[]>('GET', '/api/mcp/tools', {});
}
export async function downloadPdf(params: {
  root: string; // Root name (orca | rebuy)
  path: string; // File path or directory path relative to root
  output?: string; // merged (default) or zip
}): Promise<void> {
  const { root, path, output } = params;

  return request<void>('GET', '/api/pdf', {
    query: { root, path, output },
  });
}
export async function listPlugins(): Promise<T.PluginInfo[]> {
  return request<T.PluginInfo[]>('GET', '/api/plugins', {});
}
export async function listPluginCreds(params: {
  id: string; // Plugin ID
}): Promise<T.CredInfo[]> {
  return request<T.CredInfo[]>('GET', `/api/plugins/${params.id}/creds`, {});
}
export async function setPluginCred(params: {
  id: string; // Plugin ID
  body: T.SetCredRequest;
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('PUT', `/api/plugins/${params.id}/creds`, {
    body: params.body,
  });
}
export async function syncPluginCreds(params: {
  id: string; // Plugin ID
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('POST', `/api/plugins/${params.id}/creds/sync`, {});
}
export async function deletePluginCred(params: {
  id: string; // Plugin ID
  key: string; // Credential key
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('DELETE', `/api/plugins/${params.id}/creds/${params.key}`, {});
}
export async function getHealth(): Promise<T.HealthResponse> {
  return request<T.HealthResponse>('GET', '/api/rebuy/health/local', {});
}
export async function getSchema(): Promise<T.SchemaResponse> {
  return request<T.SchemaResponse>('GET', '/api/schema', {});
}
export async function listSchemaDatabases(): Promise<T.SchemaDbInfo[]> {
  return request<T.SchemaDbInfo[]>('GET', '/api/schema/databases', {});
}
export async function addSchemaDatabase(params: {
  body: T.SchemaDbAddRequest;
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('POST', '/api/schema/databases', {
    body: params.body,
  });
}
export async function removeSchemaDatabase(params: {
  name: string; // Database name
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('DELETE', `/api/schema/databases/${params.name}`, {});
}
export async function getSchemaDomains(): Promise<void> {
  return request<void>('GET', '/api/schema/domains', {});
}
export async function searchDocs(params: {
  q?: string; // Search query
  root?: string; // Limit search to a specific root (orca/rebuy)
}): Promise<T.SearchResult[]> {
  const { q, root } = params;

  return request<T.SearchResult[]>('GET', '/api/search', {
    query: { q, root },
  });
}
export async function listSpecs(): Promise<T.SpecMeta[]> {
  return request<T.SpecMeta[]>('GET', '/api/specs', {});
}
export async function listDbSpecs(): Promise<T.SpecInfo[]> {
  return request<T.SpecInfo[]>('GET', '/api/specs/db', {});
}
export async function registerSpec(params: { body: T.SpecRegisterRequest }): Promise<T.SpecInfo> {
  return request<T.SpecInfo>('POST', '/api/specs/register', {
    body: params.body,
  });
}
export async function refreshSpec(params: {
  name: string; // Spec name to refresh
}): Promise<T.SpecInfo> {
  return request<T.SpecInfo>('POST', `/api/specs/${params.name}/refresh`, {});
}
export async function unregisterSpec(params: {
  name: string; // Spec name to unregister
}): Promise<T.OkResponse> {
  return request<T.OkResponse>('DELETE', `/api/specs/${params.name}/unregister`, {});
}
export async function getSpec(params: {
  repo: string; // Repository name (e.g. admin-api)
  format?: string; // Response format: json (default) or yaml
  download?: boolean; // Set true to receive Content-Disposition: attachment
}): Promise<void> {
  const { format, download } = params;

  return request<void>('GET', `/api/specs/${params.repo}`, {
    query: { format, download },
  });
}
export async function downloadSpec(params: {
  repo: string; // Repository name (e.g. admin-api)
  format?: string; // json (default) or yaml
}): Promise<void> {
  const { format } = params;

  return request<void>('GET', `/api/specs/${params.repo}/download`, {
    query: { format },
  });
}
export async function getSpecGraphql(params: {
  repo: string; // Repository name (e.g. admin-api)
  download?: boolean; // Set true to receive Content-Disposition: attachment
}): Promise<void> {
  const { download } = params;

  return request<void>('GET', `/api/specs/${params.repo}/graphql`, {
    query: { download },
  });
}
export async function downloadGraphql(params: {
  repo: string; // Repository name (e.g. admin-api)
  format?: string; // sdl (default) or introspection
}): Promise<void> {
  const { format } = params;

  return request<void>('GET', `/api/specs/${params.repo}/graphql/download`, {
    query: { format },
  });
}
export async function getSpecGraphqlInfo(params: {
  repo: string; // Repository name (e.g. admin-api)
}): Promise<T.GraphQlInfo> {
  return request<T.GraphQlInfo>('GET', `/api/specs/${params.repo}/graphql/info`, {});
}
export async function proxyGraphql(params: {
  repo: string; // Repository name (e.g. shopify-admin)
  body: T.GraphqlProxyRequest;
}): Promise<void> {
  return request<void>('POST', `/api/specs/${params.repo}/graphql/proxy`, {
    body: params.body,
  });
}
export async function getSpecPublic(params: {
  repo: string; // Repository name (e.g. admin-api)
  format?: string; // Response format: json (default) or yaml
  download?: boolean; // Set true to receive Content-Disposition: attachment
}): Promise<void> {
  const { format, download } = params;

  return request<void>('GET', `/api/specs/${params.repo}/public`, {
    query: { format, download },
  });
}
/** POST /api/system/action — run install or uninstall */
export async function system_action_handler(params: {
  body: T.SystemActionRequest;
}): Promise<T.SystemActionResponse> {
  return request<T.SystemActionResponse>('POST', '/api/system/action', {
    body: params.body,
  });
}
/** GET /api/system/status — installation status for the web UI */
export async function system_status_handler(): Promise<unknown> {
  return request<unknown>('GET', '/api/system/status', {});
}
export async function runTests(params: {
  suite: string; // Test suite to run: rust | frontend | e2e | all
}): Promise<T.TestRunResponse> {
  const { suite } = params;

  return request<T.TestRunResponse>('GET', '/api/tests/run', {
    query: { suite },
  });
}
export async function getTree(params: {
  raw?: boolean; // Skip compaction — return raw filesystem tree
}): Promise<unknown> {
  const { raw } = params;

  return request<unknown>('GET', '/api/tree', {
    query: { raw },
  });
}
