// ⚠️  AUTO-GENERATED — do not edit. Run `orca gen` to regenerate.
// Source: http://localhost:12000/api/openapi.json

export interface ComponentStatus {
  installed: boolean;
  path: string;
}

export interface ConfluenceSearchQuery {
  cql?: string | null;
  limit?: number | null;
}

export interface CredInfo {
  key: string;
  synced: boolean;
  updatedAt: string;
}

export interface Ctx7Response {
  content: string;
  libraryId: string;
  title: string;
  topic?: string | null;
}

export interface DockerActionRequest {
  action: string;
  projectPath: string;
  service?: string | null;
  tail?: number | null;
}

export interface DockerActionResponse {
  composeFile?: string | null;
  output: string;
}

export interface DockerRuntimeAddRequest {
  host?: string | null;
  name: string;
  socketPath?: string | null;
  /** HTTP URL for web-based orchestrators (Dockge, Portainer) */
  url?: string | null;
}

export interface DockerRuntimeInfo {
  enabled: boolean;
  host?: string | null;
  name: string;
  socketPath?: string | null;
  /** HTTP URL for web-based orchestrators (Dockge, Portainer) */
  url?: string | null;
}

export interface DockerService {
  health: string;
  name: string;
  ports: string[];
  running: boolean;
  state: string;
}

export interface DockerServicesResponse {
  composeFile?: string | null;
  services: DockerService[];
}

export interface ErrorResponse {
  error: string;
}

export interface GraphQlEnum {
  description?: string | null;
  name: string;
  values: string[];
}

export interface GraphQlField {
  description?: string | null;
  name: string;
  required: boolean;
  typeName: string;
}

export interface GraphQlInfo {
  enums: GraphQlEnum[];
  inputs: GraphQlType[];
  mutations: GraphQlOperation[];
  queries: GraphQlOperation[];
  repo: string;
  subscriptions: GraphQlOperation[];
  types: GraphQlType[];
}

export interface GraphQlOperation {
  args: GraphQlField[];
  deprecated: boolean;
  description?: string | null;
  name: string;
  returns: string;
}

export interface GraphQlType {
  description?: string | null;
  fields: GraphQlField[];
  name: string;
}

export interface GraphqlDownloadQuery {
  /** sdl (default) or introspection */
  format?: string | null;
}

export interface GraphqlProxyRequest {
  /** Operation name */
  operationName?: string | null;
  /** GraphQL query or mutation document */
  query: string;
  /** Shopify shop domain (e.g. "myshop.myshopify.com" or "myshop") */
  shop: string;
  /** Shopify Admin API access token */
  token: string;
  /** Query variables */
  variables?: unknown;
}

export interface HealthCheck {
  label: string;
  ok: boolean;
  output: string;
  tool: string;
}

export interface HealthResponse {
  checks: HealthCheck[];
  timestamp: string;
}

export interface JiraIssuesQuery {
  jql?: string | null;
  maxResults?: number | null;
}

export interface LogProject {
  path: string;
  project: string;
  services: LogService[];
}

export interface LogService {
  health: string;
  name: string;
  ports: string[];
  running: boolean;
  state: string;
}

export interface LogServicesResponse {
  projects: LogProject[];
}

export interface LogsResponse {
  output: string;
}

export interface MapRequest {
  external_tool: string;
  name: string;
  orca_tool: string;
}

export interface MappingRow {
  confidence?: number | null;
  enabled: boolean;
  external_tool: string;
  match_type: string;
  mcp_name: string;
  orca_tool: string;
}

export interface McpContent {
  text: string;
  type: string;
}

export interface McpRunRequest {
  arguments?: unknown;
  name: string;
  server: string;
}

export interface McpRunResponse {
  content: McpContent[];
  isError?: boolean | null;
}

export interface McpServerAddRequest {
  args?: string[];
  command: string;
  env?: Record<string, string>;
  name: string;
}

export interface McpServerInfo {
  args: string[];
  command: string;
  enabled: boolean;
  env: Record<string, string>;
  name: string;
}

export interface McpToolInfo {
  description: string;
  inputSchema: unknown;
  name: string;
  server: string;
}

export interface MpcStatus {
  registered: boolean;
}

export type NodeType = "file" | "dir";

export interface OkResponse {
  ok: boolean;
}

export interface PdfQuery {
  /** merged (default) | zip */
  output?: string | null;
  path: string;
  root: string;
}

export interface PluginInfo {
  description: string;
  enabled: boolean;
  id: string;
  mcpCommand?: string | null;
  tier: string;
}

export interface PrQuery {
  slug: string;
  workspace: string;
}

export interface ProgressRequest {
  page: string;
}

export interface ProgressResponse {
  page?: string | null;
}

export interface RepoInfo {
  remote: string;
  slug: string;
  workspace: string;
}

export interface SchemaDbAddRequest {
  container?: string | null;
  database: string;
  domainsFile?: string | null;
  host?: string | null;
  name: string;
  password: string;
  port?: number | null;
  user: string;
}

export interface SchemaDbInfo {
  container?: string | null;
  database: string;
  domainsFile?: string | null;
  enabled: boolean;
  host?: string | null;
  name: string;
  port?: number | null;
  user: string;
}

export interface SchemaResponse {
  errors?: string[] | null;
  showTabs: boolean;
  tabs: SchemaTab[];
}

export interface SchemaTab {
  columns: unknown;
  domains: unknown;
  foreignKeys: unknown[];
  tables: unknown[];
  title: string;
}

export interface SearchResult {
  matches: string[];
  path: string;
  root: string;
}

export interface SetCredRequest {
  key: string;
  value: string;
}

export interface SpecDownloadQuery {
  /** json (default) or yaml */
  format?: string | null;
}

export interface SpecFiles {
  full?: string | null;
  public?: string | null;
}

export interface SpecInfo {
  cachedAt?: string | null;
  enabled: boolean;
  name: string;
  pathCount?: number | null;
  sourceMcp?: string | null;
  url?: string | null;
}

export interface SpecMeta {
  baseUrl?: string | null;
  capturedAt?: string | null;
  description?: string | null;
  files?: SpecFiles | null;
  notes?: string | null;
  pathCount?: number | null;
  project: string;
  repo: string;
  source: string;
}

export interface SpecQuery {
  /** true adds Content-Disposition: attachment header */
  download?: boolean | null;
  /** "json" (default) or "yaml" */
  format?: string | null;
}

export interface SpecRegisterRequest {
  name: string;
  url: string;
}

export interface SystemActionRequest {
  /** "install" or "uninstall" */
  action: string;
}

export interface SystemActionResponse {
  done: string[];
  errors: string[];
  ok: boolean;
  skipped: string[];
}

export interface SystemStatusResponse {
  agents: ComponentStatus;
  binary: ComponentStatus;
  claude_md: ComponentStatus;
  mcp: MpcStatus;
  vault: ComponentStatus;
}

export interface TestRunQuery {
  /** Which suite to run: rust | frontend | e2e | all */
  suite: string;
}

export interface TestRunResponse {
  duration_ms: number;
  exit_code: number;
  failed: number;
  output: string;
  passed: number;
  suite: string;
}

export interface TransitionBody {
  transitionId: string;
}

export interface TreeNode {
  children?: TreeNode[] | null;
  name: string;
  path: string;
  type: NodeType;
  order?: number | null;
}
