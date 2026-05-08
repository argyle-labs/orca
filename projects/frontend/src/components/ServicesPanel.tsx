import { useState, useMemo } from 'react';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  Modal, Button, Badge, Group, Stack, Select, TextInput,
  ScrollArea, ActionIcon, Text, Loader, Tabs, CopyButton, Tooltip,
} from '@mantine/core';
import { notifications } from '@mantine/notifications';

const SERVER = 'rebuy-cli';
const POLL_MS = 30_000;
const HOME = '/Users/scottkey';

// ── Types ─────────────────────────────────────────────────────────────────────

interface Param {
  key: string;
  label: string;
  type: 'text' | 'select';
  options?: string[];
  required?: boolean;
  default?: string;
}

interface Action {
  label: string;
  tool: string;
  params?: Param[];
  dangerous?: boolean;
  primary?: boolean;
}

interface ServiceDef {
  id: string;
  label: string;
  statusTool: string;
  actions: Action[];
}

interface ServiceState {
  ok: boolean | null;
  output: string;
  badge: string | null;
}

interface Project {
  name: string;
  running: boolean;
  path: string;
}

interface DockerService {
  name: string;
  state: string;
  running: boolean;
  health: string;
  ports: string[];
}

// ── Service definitions ───────────────────────────────────────────────────────

const SERVICES: ServiceDef[] = [
  {
    id: 'db', label: 'DB', statusTool: 'rebuy_db_status',
    actions: [
      { label: 'Start',    tool: 'rebuy_db_up',       primary: true },
      { label: 'Stop',     tool: 'rebuy_db_down',      primary: true },
      { label: 'Status',   tool: 'rebuy_db_status' },
      { label: 'Health',   tool: 'rebuy_db_health' },
      { label: 'Migrate',  tool: 'rebuy_db_migrate' },
      { label: 'Logs',     tool: 'rebuy_db_logs' },
      { label: 'Current',  tool: 'rebuy_db_current' },
      { label: 'List',     tool: 'rebuy_db_list' },
      { label: 'Install',  tool: 'rebuy_db_install' },
      { label: 'Download', tool: 'rebuy_db_download' },
      { label: 'Reset',    tool: 'rebuy_db_reset', dangerous: true },
      {
        label: 'Switch', tool: 'rebuy_db_switch', dangerous: true,
        params: [{ key: 'profile', label: 'Profile', type: 'select', options: ['local', 'stage', 'prod', 'prod-primary'], required: true }],
      },
    ],
  },
  {
    id: 'env', label: 'Env', statusTool: 'rebuy_env_status',
    actions: [
      { label: 'Start',      tool: 'rebuy_env_start',      primary: true },
      { label: 'Stop',       tool: 'rebuy_env_stop',       primary: true },
      { label: 'Restart',    tool: 'rebuy_env_restart',    primary: true },
      { label: 'Status',     tool: 'rebuy_env_status' },
      { label: 'Logs',       tool: 'rebuy_env_logs' },
      { label: 'Current',    tool: 'rebuy_env_current' },
      { label: 'History',    tool: 'rebuy_env_history' },
      { label: 'Generate',   tool: 'rebuy_env_generate' },
      { label: 'Validate',   tool: 'rebuy_env_validate' },
      { label: 'Dev',        tool: 'rebuy_env_dev' },
      { label: 'DNS Dev',    tool: 'rebuy_env_dns_dev' },
      { label: 'DNS Prod',   tool: 'rebuy_env_dns_prod' },
      { label: 'DNS Status', tool: 'rebuy_env_dns_status' },
    ],
  },
  {
    id: 'engines', label: 'Engines', statusTool: 'rebuy_engines_status',
    actions: [
      { label: 'Start',  tool: 'rebuy_engines_start',  primary: true },
      { label: 'Stop',   tool: 'rebuy_engines_stop',   primary: true },
      { label: 'Status', tool: 'rebuy_engines_status' },
      { label: 'List',   tool: 'rebuy_engines_list' },
      {
        label: 'Switch', tool: 'rebuy_engines_switch', dangerous: true,
        params: [{ key: 'cluster', label: 'Cluster', type: 'select', options: ['staging', 'prod'], required: true }],
      },
    ],
  },
  {
    id: 'tunnel', label: 'Tunnel', statusTool: 'rebuy_tunnel_status',
    actions: [
      { label: 'Start',  tool: 'rebuy_tunnel_start',  primary: true },
      { label: 'Stop',   tool: 'rebuy_tunnel_stop',   primary: true },
      { label: 'Status', tool: 'rebuy_tunnel_status' },
      {
        label: 'Extend', tool: 'rebuy_tunnel_extend',
        params: [
          { key: 'profile', label: 'Profile', type: 'text', required: false },
          { key: 'minutes', label: 'Minutes', type: 'text', default: '60', required: false },
        ],
      },
    ],
  },
  {
    id: 'network', label: 'Network', statusTool: 'rebuy_network_status',
    actions: [
      { label: 'Status', tool: 'rebuy_network_status', primary: true },
      { label: 'Create', tool: 'rebuy_network_create' },
      { label: 'Remove', tool: 'rebuy_network_remove', dangerous: true },
    ],
  },
];

// ── Helpers ───────────────────────────────────────────────────────────────────

function stripAnsi(s: string): string {
  return s.replace(/(\x1b|\x1B)\[[\d;]*m|\[[\d;]+m/g, '');
}

function parseStatus(output: string): boolean | null {
  if (!output) return null;
  const lower = stripAnsi(output).toLowerCase();
  if (/\b(stopped|not running|not found|not connected|not started|down|failed|error)\b/.test(lower)) return false;
  if (output.includes('"success": false')) return false;
  if (/\b(running|up|healthy|connected|active|started)\b/.test(lower)) return true;
  if (output.includes('"success": true')) return true;
  return null;
}

function parseBadge(id: string, output: string): string | null {
  const clean = stripAnsi(output);
  if (id === 'engines') {
    const m = clean.match(/cluster:\s*(\w+)/i);
    return m ? m[1] : null;
  }
  if (id === 'env') {
    const m = clean.match(/Active Database Profile:[\s\S]{0,120}?(local|stage|prod(?:-primary)?)/i);
    return m ? m[1] : null;
  }
  if (id === 'tunnel') {
    const m = clean.match(/cluster:\s*(\w+)/i) ?? clean.match(/profile:\s*(\w[\w-]*)/i);
    return m ? m[1] : null;
  }
  return null;
}

function badgeTier(badge: string): 'danger' | 'warn' | 'dim' {
  if (/^prod/i.test(badge)) return 'danger';
  if (/^stag/i.test(badge)) return 'warn';
  return 'dim';
}

function badgeColor(tier: 'danger' | 'warn' | 'dim') {
  if (tier === 'danger') return 'red';
  if (tier === 'warn') return 'yellow';
  return 'gray';
}

async function fetchDockerEngine(): Promise<{ engine: string; running: boolean }> {
  const res = await fetch('/api/docker/engine');
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

async function startDockerEngine(): Promise<string> {
  const res = await fetch('/api/docker/engine/start', { method: 'POST' });
  const data = await res.json();
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  return data.output ?? '';
}

async function callTool(tool: string, args: Record<string, unknown> = {}): Promise<string> {
  const res = await fetch('/api/mcp/run', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ server: SERVER, name: tool, arguments: args }),
  });
  const data = await res.json();
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  return data.content?.[0]?.text ?? JSON.stringify(data);
}

async function fetchServiceStatus(svc: ServiceDef): Promise<ServiceState> {
  const output = await callTool(svc.statusTool);
  return { ok: parseStatus(output), output, badge: parseBadge(svc.id, output) };
}

function parseProjectList(output: string): Project[] {
  const projects: Project[] = [];
  let current: Partial<Project> | null = null;
  for (const line of output.split('\n')) {
    const clean = line.replace(/\x1b\[[0-9;]*m/g, '').trim();
    const nameMatch = clean.match(/^📦\s+(.+)$/);
    if (nameMatch) {
      if (current?.name && current?.path) projects.push({ running: false, ...current } as Project);
      current = { name: nameMatch[1].trim() };
      continue;
    }
    const pathMatch = clean.match(/^Path:\s+(.+)$/);
    if (pathMatch && current) current.path = pathMatch[1].trim();
  }
  if (current?.name && current?.path) projects.push({ running: false, ...current } as Project);
  return projects;
}

async function fetchProjects(): Promise<Project[]> {
  const output = await callTool('rebuy_project_list', { all: true });
  const parsed = parseProjectList(output);
  const checks = parsed.map(async (proj) => {
    try {
      const res = await fetch(`/api/docker/services?path=${encodeURIComponent(proj.path)}`);
      const data = await res.json();
      const running = (data.services ?? []).some((s: DockerService) => s.running);
      return { ...proj, running };
    } catch {
      return proj;
    }
  });
  const results = await Promise.allSettled(checks);
  return results.map((r, i) => r.status === 'fulfilled' ? r.value : parsed[i]);
}

async function fetchDockerServices(path: string): Promise<{ composeFile: string | null; services: DockerService[] }> {
  const res = await fetch(`/api/docker/services?path=${encodeURIComponent(path)}`);
  const data = await res.json();
  return { composeFile: data.composeFile ?? null, services: data.services ?? [] };
}

async function dockerAction(projectPath: string, action: string, service?: string, tail?: number): Promise<string> {
  const res = await fetch('/api/docker/action', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ projectPath, service, action, tail }),
  });
  const data = await res.json();
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  return data.output ?? '';
}

// ── Dot indicator ─────────────────────────────────────────────────────────────

function StatusDot({ ok }: { ok: boolean | null }) {
  const cls = ok === true ? 'sp-dot sp-dot-ok' : ok === false ? 'sp-dot sp-dot-fail' : 'sp-dot sp-dot-unknown';
  return <span className={cls}>●</span>;
}

// ── Service modal ─────────────────────────────────────────────────────────────

function ServiceModal({ svc, status, opened, onClose, onStatusChange }: {
  svc: ServiceDef;
  status: ServiceState;
  opened: boolean;
  onClose: () => void;
  onStatusChange: (patch: Partial<ServiceState>) => void;
}) {
  const [output, setOutput] = useState('');
  const [pendingAction, setPendingAction] = useState<Action | null>(null);
  const [params, setParams] = useState<Record<string, string>>({});

  const runMutation = useMutation({
    mutationFn: ({ action, args }: { action: Action; args: Record<string, string> }) => {
      const cleaned: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(args)) if (v !== '') cleaned[k] = v;
      return callTool(action.tool, cleaned);
    },
    onSuccess: async (text, { action }) => {
      setOutput(text);
      setPendingAction(null);
      if (action.tool === svc.statusTool || action.label === 'Status') {
        onStatusChange({ ok: parseStatus(text), output: text });
      } else {
        await new Promise((r) => setTimeout(r, 1200));
        try {
          const statusText = await callTool(svc.statusTool);
          onStatusChange({ ok: parseStatus(statusText), output: statusText });
        } catch {}
      }
    },
    onError: (e: any) => {
      setOutput(`Error: ${e.message ?? 'unknown'}`);
    },
  });

  function initAction(action: Action) {
    const defaults: Record<string, string> = {};
    action.params?.forEach((p) => { if (p.default) defaults[p.key] = p.default; });
    if (action.params?.some((p) => p.required)) {
      setParams(defaults);
      setPendingAction(action);
    } else {
      runMutation.mutate({ action, args: defaults });
    }
  }

  const busy = runMutation.isPending;
  const primaryActions = svc.actions.filter((a) => a.primary);
  const moreActions = svc.actions.filter((a) => !a.primary);

  return (
    <Modal
      opened={opened}
      onClose={onClose}
      zIndex={400}
      closeOnClickOutside={false}
      closeOnEscape={true}
      title={
        <Group gap="xs">
          <StatusDot ok={status.ok} />
          <Text fw={600}>{svc.label}</Text>
          <Text size="xs" c="dimmed">rebuy-cli</Text>
        </Group>
      }
      size="80%"
      styles={{
        content: { height: '85vh', display: 'flex', flexDirection: 'column' },
        body: { flex: 1, display: 'flex', flexDirection: 'column', minHeight: 0, overflow: 'hidden' },
      }}
    >
      <Stack gap="md" style={{ flex: 1, minHeight: 0 }}>
        <div>
          <Text size="xs" c="dimmed" mb={6}>Primary</Text>
          <Group gap="xs">
            {primaryActions.map((action) => (
              <Button
                key={action.tool}
                size="xs"
                variant="light"
                color={action.dangerous ? 'red' : 'violet'}
                disabled={busy}
                loading={runMutation.isPending && runMutation.variables?.action.tool === action.tool}
                onClick={() => initAction(action)}
              >
                {action.label}
              </Button>
            ))}
          </Group>
        </div>

        {moreActions.length > 0 && (
          <div>
            <Text size="xs" c="dimmed" mb={6}>All actions</Text>
            <Group gap="xs">
              {moreActions.map((action) => (
                <Button
                  key={action.tool}
                  size="xs"
                  variant="subtle"
                  color={action.dangerous ? 'red' : 'gray'}
                  disabled={busy}
                  loading={runMutation.isPending && runMutation.variables?.action.tool === action.tool}
                  onClick={() => initAction(action)}
                >
                  {action.label}
                </Button>
              ))}
            </Group>
          </div>
        )}

        {pendingAction && (
          <Stack gap="xs">
            <Text size="xs" fw={600}>Configure: {pendingAction.label}</Text>
            {pendingAction.params?.map((p) => (
              p.type === 'select' ? (
                <Select
                  key={p.key}
                  label={p.label + (p.required ? ' *' : '')}
                  size="xs"
                  data={p.options ?? []}
                  value={params[p.key] ?? ''}
                  onChange={(v) => setParams((prev) => ({ ...prev, [p.key]: v ?? '' }))}
                />
              ) : (
                <TextInput
                  key={p.key}
                  label={p.label + (p.required ? ' *' : '')}
                  size="xs"
                  placeholder={p.default ?? ''}
                  value={params[p.key] ?? ''}
                  onChange={(e) => setParams((prev) => ({ ...prev, [p.key]: e.target.value }))}
                />
              )
            ))}
            <Group gap="xs">
              <Button
                size="xs"
                color="violet"
                disabled={pendingAction.params?.some((p) => p.required && !params[p.key])}
                loading={busy}
                onClick={() => runMutation.mutate({ action: pendingAction, args: params })}
              >
                Run
              </Button>
              <Button size="xs" variant="subtle" color="gray" onClick={() => setPendingAction(null)}>Cancel</Button>
            </Group>
          </Stack>
        )}

        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', minHeight: 0 }}>
          <Text size="xs" c="dimmed" mb={4}>
            {busy
              ? `Running ${runMutation.variables?.action.tool}…`
              : (output || status.output) ? 'Output' : 'No output yet'}
          </Text>
          <ScrollArea h="100%" style={{ flex: 1, minHeight: 0 }} type="auto">
            <pre className="svc-output" style={{ height: '100%', boxSizing: 'border-box' }}>
              {output || (status.output ? `[status]\n${status.output}` : '')}
            </pre>
          </ScrollArea>
        </div>
      </Stack>
    </Modal>
  );
}

// ── Project modal ─────────────────────────────────────────────────────────────

const LOG_TAIL_OPTIONS = ['50', '100', '200', '500', '1000'];

function ProjectModal({ project, opened, onClose, onReload }: {
  project: Project;
  opened: boolean;
  onClose: () => void;
  onReload: () => void;
}) {
  const [fullScreen, setFullScreen] = useState(false);
  const [logTab, setLogTab] = useState<string>('all');
  const [logSearch, setLogSearch] = useState('');
  const [logTail, setLogTail] = useState('200');
  const [actionOutput, setActionOutput] = useState('');
  const [actionLabel, setActionLabel] = useState('');

  const qc = useQueryClient();
  const servicesKey = ['docker-services', project.path];

  const { data: dockerData, isFetching: loadingServices, refetch: refreshServices } = useQuery({
    queryKey: servicesKey,
    queryFn: () => fetchDockerServices(project.path),
    enabled: opened,
  });

  const composeFile = dockerData?.composeFile ?? null;
  const services = dockerData?.services ?? [];

  // Auto-fetch logs for the active tab; refresh every 15s
  const logsQuery = useQuery({
    queryKey: ['project-logs', project.path, logTab, logTail],
    queryFn: () => dockerAction(project.path, 'logs', logTab === 'all' ? undefined : logTab, Number(logTail)),
    enabled: opened,
    refetchInterval: 15_000,
    staleTime: 10_000,
  });

  const filteredLogs = useMemo(() => {
    const text = logsQuery.data ?? '';
    if (!logSearch.trim()) return text;
    const lower = logSearch.toLowerCase();
    return text.split('\n').filter((l) => l.toLowerCase().includes(lower)).join('\n');
  }, [logsQuery.data, logSearch]);

  const mcpMutation = useMutation({
    mutationFn: ({ tool, args }: { tool: string; args?: Record<string, unknown> }) =>
      callTool(tool, { path: project.path, ...args }),
    onSuccess: (text, { tool }) => {
      setActionOutput(text);
      setActionLabel(tool.replace('rebuy_project_', ''));
      setTimeout(() => { refreshServices(); onReload(); }, 1200);
    },
    onError: (e: any, { tool }) => {
      notifications.show({ color: 'red', message: `${project.name} — ${tool}: ${e.message ?? 'failed'}` });
    },
  });

  const dockerMutation = useMutation({
    mutationFn: ({ action, service, tail }: { action: string; service?: string; tail?: number }) =>
      dockerAction(project.path, action, service, tail),
    onSuccess: (text, { action, service }) => {
      if (action === 'logs') {
        // Switch log tab to that service rather than clobbering action output
        if (service) setLogTab(service);
        qc.invalidateQueries({ queryKey: ['project-logs', project.path] });
      } else {
        setActionOutput(text);
        setActionLabel(service ? `${service} ${action}` : action);
        if (['start', 'stop', 'restart', 'up', 'down', 'build'].includes(action)) {
          setTimeout(() => { refreshServices(); onReload(); qc.invalidateQueries({ queryKey: ['projects'] }); }, 1200);
        }
      }
    },
    onError: (e: any, { action, service }) => {
      notifications.show({ color: 'red', message: `${service ?? project.name} ${action}: ${e.message ?? 'failed'}` });
    },
  });

  const busy = mcpMutation.isPending || dockerMutation.isPending;
  const logH = fullScreen ? 'calc(100vh - 420px)' : 260;

  return (
    <Modal
      opened={opened}
      onClose={onClose}
      fullScreen={fullScreen}
      size="xl"
      zIndex={400}
      title={
        <Group gap="xs" style={{ flex: 1 }}>
          <StatusDot ok={project.running} />
          <Text fw={600}>{project.name}</Text>
          <Text size="xs" c="dimmed" title={project.path} style={{ flex: 1 }}>
            {project.path.replace(HOME + '/', '~/')}
          </Text>
          <ActionIcon
            size="sm"
            variant="subtle"
            title={fullScreen ? 'Restore' : 'Expand to full page'}
            onClick={() => setFullScreen((f) => !f)}
          >
            {fullScreen ? '⤡' : '⤢'}
          </ActionIcon>
        </Group>
      }
    >
      <Stack gap="md">
        {/* Project-level actions */}
        <div>
          <Text size="xs" c="dimmed" mb={6}>Project (rebuy-cli)</Text>
          <Group gap="xs">
            {[
              { label: 'Start All', tool: 'rebuy_project_start' },
              { label: 'Stop All',  tool: 'rebuy_project_stop' },
              { label: 'Restart',   tool: 'rebuy_project_restart' },
              { label: 'Status',    tool: 'rebuy_project_status' },
              { label: 'Logs',      tool: 'rebuy_project_logs' },
            ].map(({ label, tool }) => (
              <Button
                key={tool}
                size="xs"
                variant="light"
                color="violet"
                disabled={busy}
                loading={mcpMutation.isPending && mcpMutation.variables?.tool === tool}
                onClick={() => mcpMutation.mutate({ tool })}
              >
                {label}
              </Button>
            ))}
          </Group>
        </div>

        {/* Last action output (collapsible) */}
        {actionOutput && (
          <div>
            <Group gap="xs" mb={4}>
              <Text size="xs" c="dimmed">{actionLabel}</Text>
              <CopyButton value={actionOutput}>
                {({ copied, copy }) => (
                  <Tooltip label={copied ? 'Copied!' : 'Copy'} withArrow>
                    <ActionIcon size="xs" variant="subtle" onClick={copy} color={copied ? 'teal' : 'gray'}>
                      {copied ? '✓' : '⎘'}
                    </ActionIcon>
                  </Tooltip>
                )}
              </CopyButton>
              <ActionIcon size="xs" variant="subtle" color="gray" onClick={() => setActionOutput('')}>✕</ActionIcon>
            </Group>
            <ScrollArea h={120}>
              <pre className="svc-output">{actionOutput}</pre>
            </ScrollArea>
          </div>
        )}

        {/* Docker Compose services */}
        <div>
          <Group gap="xs" mb={6}>
            <Text size="xs" c="dimmed">Services (docker compose)</Text>
            {composeFile && <Text size="xs" c="dimmed" opacity={0.5}>{composeFile.split('/').pop()}</Text>}
            <ActionIcon size="xs" variant="subtle" onClick={() => refreshServices()} loading={loadingServices}>↺</ActionIcon>
          </Group>

          {services.length === 0 && !loadingServices && (
            <Text size="xs" c="dimmed">{composeFile ? 'No services found' : 'No compose file in this directory'}</Text>
          )}

          <Stack gap={4}>
            {services.map((svc) => (
              <div key={svc.name} className="proj-svc-row">
                <div className="proj-svc-header">
                  <StatusDot ok={svc.running} />
                  <span className="proj-svc-name">{svc.name}</span>
                  {svc.health && <span className="proj-svc-health">{svc.health}</span>}
                  {svc.ports.length > 0 && <span className="proj-svc-ports">{svc.ports.slice(0, 3).join(', ')}</span>}
                  <Group gap={4} ml="auto">
                    {[
                      { title: 'Up',      action: 'up',      icon: '▶' },
                      { title: 'Stop',    action: 'stop',    icon: '■' },
                      { title: 'Restart', action: 'restart', icon: '↺' },
                      { title: 'Build',   action: 'build',   icon: '🔨' },
                      { title: 'Logs',    action: 'logs',    icon: '📋' },
                    ].map(({ title, action, icon }) => (
                      <ActionIcon
                        key={action}
                        size="xs"
                        variant="subtle"
                        title={title}
                        disabled={busy}
                        loading={dockerMutation.isPending && dockerMutation.variables?.action === action && dockerMutation.variables?.service === svc.name}
                        onClick={() => dockerMutation.mutate({ action, service: svc.name, tail: Number(logTail) })}
                      >
                        {icon}
                      </ActionIcon>
                    ))}
                  </Group>
                </div>
              </div>
            ))}
          </Stack>
        </div>

        {/* Logs section — tabs + search + copy */}
        <div>
          <Tabs value={logTab} onChange={(v) => setLogTab(v ?? 'all')} mb="xs">
            <Tabs.List>
              <Tabs.Tab value="all">All</Tabs.Tab>
              {services.map((svc) => (
                <Tabs.Tab key={svc.name} value={svc.name}>
                  <Group gap={4}>
                    <StatusDot ok={svc.running} />
                    {svc.name}
                  </Group>
                </Tabs.Tab>
              ))}
            </Tabs.List>
          </Tabs>

          <Group gap="xs" mb="xs">
            <TextInput
              size="xs"
              placeholder="Filter logs…"
              value={logSearch}
              onChange={(e) => setLogSearch(e.target.value)}
              style={{ flex: 1 }}
            />
            <Select
              size="xs"
              data={LOG_TAIL_OPTIONS}
              value={logTail}
              onChange={(v) => setLogTail(v ?? '200')}
              w={70}
              title="Lines"
            />
            <Tooltip label="Refresh" withArrow>
              <ActionIcon size="sm" variant="subtle" onClick={() => logsQuery.refetch()} loading={logsQuery.isFetching}>
                ↺
              </ActionIcon>
            </Tooltip>
            <CopyButton value={filteredLogs}>
              {({ copied, copy }) => (
                <Tooltip label={copied ? 'Copied!' : 'Copy logs'} withArrow>
                  <ActionIcon size="sm" variant="subtle" onClick={copy} color={copied ? 'teal' : 'gray'}>
                    {copied ? '✓' : '⎘'}
                  </ActionIcon>
                </Tooltip>
              )}
            </CopyButton>
          </Group>

          <ScrollArea h={logH}>
            <pre className="svc-output">
              {logsQuery.isFetching && !logsQuery.data
                ? 'Loading…'
                : filteredLogs || 'No logs'}
            </pre>
          </ScrollArea>
        </div>
      </Stack>
    </Modal>
  );
}

// ── Projects section ──────────────────────────────────────────────────────────

function ProjectsSection() {
  const [open, setOpen] = useState(true);
  const [openProject, setOpenProject] = useState<Project | null>(null);
  const qc = useQueryClient();

  const { data: projects = [], isFetching, refetch } = useQuery({
    queryKey: ['projects'],
    queryFn: fetchProjects,
    refetchInterval: POLL_MS,
    staleTime: POLL_MS - 5_000,
  });

  const actionMutation = useMutation({
    mutationFn: ({ project, tool }: { project: Project; tool: string }) =>
      callTool(tool, { path: project.path }),
    onSuccess: (output, { project, tool }) => {
      const isError = /\b(error|failed|fatal)\b/i.test(output) && !/no error/i.test(output);
      if (isError) notifications.show({ color: 'red', message: `${project.name}: ${output.slice(0, 200)}` });
      setTimeout(() => { qc.invalidateQueries({ queryKey: ['projects'] }); }, 1500);
    },
    onError: (e: any, { project, tool }) => {
      notifications.show({ color: 'red', message: `${project.name} — ${tool.replace('rebuy_project_', '')}: ${e.message ?? 'failed'}` });
    },
  });

  return (
    <>
      <div className="sp-projects-header">
        <button className="sp-projects-toggle" onClick={() => setOpen((o) => !o)}>
          <span className="tree-arrow">{open ? '▾' : '▸'}</span>
          <span className="sp-title">Projects</span>
        </button>
        <button className="sp-detail-btn" onClick={() => refetch()} disabled={isFetching}>
          {isFetching ? <Loader size={10} color="gray" /> : '↺'}
        </button>
      </div>

      {open && (
        <div className="sp-projects-list">
          {projects.length === 0 && !isFetching && (
            <div className="sp-projects-empty">No projects found</div>
          )}
          {projects.map((proj) => {
            const pendingTool = actionMutation.isPending && actionMutation.variables?.project.name === proj.name
              ? actionMutation.variables.tool
              : null;
            const busy = pendingTool !== null;
            return (
              <div key={proj.name} className="sp-row">
                <StatusDot ok={proj.running} />
                <button className="sp-label-btn" onClick={() => setOpenProject(proj)} title={proj.path}>
                  {proj.name}
                </button>
                <div className="sp-actions">
                  {[
                    { title: 'Start',   tool: 'rebuy_project_start',   icon: '▶' },
                    { title: 'Stop',    tool: 'rebuy_project_stop',    icon: '■' },
                    { title: 'Restart', tool: 'rebuy_project_restart', icon: '↺' },
                  ].map(({ title, tool, icon }) => (
                    <button
                      key={tool}
                      className="sp-btn"
                      title={title}
                      disabled={busy}
                      onClick={() => actionMutation.mutate({ project: proj, tool })}
                    >
                      {pendingTool === tool ? '…' : icon}
                    </button>
                  ))}
                  <button className="sp-btn sp-btn-more" title="Details" onClick={() => setOpenProject(proj)}>···</button>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {openProject && (
        <ProjectModal
          project={openProject}
          opened={!!openProject}
          onClose={() => setOpenProject(null)}
          onReload={() => qc.invalidateQueries({ queryKey: ['projects'] })}
        />
      )}
    </>
  );
}

// ── Docker engine row ─────────────────────────────────────────────────────────

function DockerEngineRow() {
  const qc = useQueryClient();

  const { data, isFetching } = useQuery({
    queryKey: ['docker-engine'],
    queryFn: fetchDockerEngine,
    refetchInterval: POLL_MS,
    staleTime: POLL_MS - 5_000,
    placeholderData: { engine: 'unknown', running: false },
  });

  const startMutation = useMutation({
    mutationFn: startDockerEngine,
    onSuccess: () => {
      setTimeout(() => qc.invalidateQueries({ queryKey: ['docker-engine'] }), 2000);
    },
    onError: (e: any) => {
      notifications.show({ color: 'red', message: `Docker engine start: ${e.message ?? 'failed'}` });
    },
  });

  const running = data?.running ?? false;
  const engine = data?.engine ?? 'unknown';

  return (
    <div className="sp-row sp-row-engine">
      <StatusDot ok={running} />
      <span className="sp-label">docker ({engine})</span>
      {isFetching && !startMutation.isPending && <Loader size={10} color="gray" />}
      {!running && (
        <button
          className="sp-btn"
          title="Start Docker engine"
          disabled={startMutation.isPending}
          onClick={() => startMutation.mutate()}
        >
          {startMutation.isPending ? '…' : '▶'}
        </button>
      )}
    </div>
  );
}

// ── Main panel ────────────────────────────────────────────────────────────────

export function ServicesPanel({ onDetailOpen }: { onDetailOpen: () => void }) {
  const [openModal, setOpenModal] = useState<string | null>(null);
  const qc = useQueryClient();

  const { data: mode } = useQuery({
    queryKey: ['mode'],
    queryFn: () => callTool('rebuy_mode_current').then((m) => m.trim()),
    refetchInterval: POLL_MS,
  });

  const serviceQueries = SERVICES.map((svc) => ({
    svc,
    query: useQuery({
      queryKey: ['service', svc.id],
      queryFn: () => fetchServiceStatus(svc),
      refetchInterval: POLL_MS,
      staleTime: POLL_MS - 5_000,
      placeholderData: { ok: null, output: '', badge: null },
    }),
  }));

  const primaryMutation = useMutation({
    mutationFn: ({ svc, tool }: { svc: ServiceDef; tool: string }) => callTool(tool),
    onSuccess: (output, { svc, tool }) => {
      const isError = /\b(error|failed|fatal)\b/i.test(output) && !/no error/i.test(output);
      if (isError) notifications.show({ color: 'red', message: `${svc.label}: ${output.slice(0, 200)}` });
      setTimeout(() => qc.invalidateQueries({ queryKey: ['service', svc.id] }), 1500);
    },
    onError: (e: any, { svc, tool }) => {
      notifications.show({ color: 'red', message: `${svc.label} — ${tool.replace('rebuy_', '')}: ${e.message ?? 'failed'}` });
    },
  });

  const activeSvc = openModal ? SERVICES.find((s) => s.id === openModal) : null;
  const activeState = activeSvc ? (serviceQueries.find((q) => q.svc.id === activeSvc.id)?.query.data ?? { ok: null, output: '', badge: null }) : null;

  return (
    <>
      <div className="sp-panel">
        <div className="sp-header">
          <span className="sp-title">Services</span>
          {mode && <span className={`sp-badge sp-badge-${badgeTier(mode) === 'dim' ? 'dim' : badgeTier(mode)}`}>{mode}</span>}
          <button className="sp-detail-btn" onClick={onDetailOpen}>Details ↗</button>
        </div>

        <DockerEngineRow />

        {serviceQueries.map(({ svc, query }) => {
          const state = query.data ?? { ok: null, output: '', badge: null };
          const isPending = primaryMutation.isPending && primaryMutation.variables?.svc.id === svc.id;
          const primaryActions = svc.actions.filter((a) => a.primary && !a.params?.length);

          return (
            <div key={svc.id} className="sp-row">
              <StatusDot ok={state.ok} />
              <button className="sp-label-btn" onClick={() => setOpenModal(svc.id)} title={state.output || undefined}>
                {svc.label}
              </button>
              {state.badge && (
                <Badge
                  size="xs"
                  color={badgeColor(badgeTier(state.badge))}
                  variant="light"
                  style={{ cursor: 'default' }}
                >
                  {state.badge}
                </Badge>
              )}
              <div className="sp-actions">
                {primaryActions.map((action) => (
                  <button
                    key={action.tool}
                    className="sp-btn"
                    title={action.label}
                    disabled={isPending}
                    onClick={() => primaryMutation.mutate({ svc, tool: action.tool })}
                  >
                    {isPending && primaryMutation.variables?.tool === action.tool ? '…' :
                      action.label === 'Start' ? '▶' :
                      action.label === 'Stop' ? '■' :
                      action.label === 'Restart' ? '↺' : action.label[0]}
                  </button>
                ))}
                <button className="sp-btn sp-btn-more" title="All actions" onClick={() => setOpenModal(svc.id)}>···</button>
              </div>
            </div>
          );
        })}
      </div>

      <ProjectsSection />

      {activeSvc && activeState && (
        <ServiceModal
          svc={activeSvc}
          status={activeState}
          opened={!!openModal}
          onClose={() => setOpenModal(null)}
          onStatusChange={(patch) => {
            qc.setQueryData(['service', activeSvc.id], (prev: ServiceState) => ({ ...prev, ...patch }));
          }}
        />
      )}
    </>
  );
}
