<script lang="ts">
  import { onMount } from 'svelte';
  import StatusDot from './StatusDot.svelte';
  import LogViewer from './LogViewer.svelte';
  import { callTool, parseStatus, parseBadge, badgeTier, stripAnsi } from '../mcp';
  import { getAllProjects, setLinkedProjectNames } from '../stores/sidebar.svelte';
  import { listSpecs } from '../api/client';
  import type { Project, DockerService } from '../types/sidebar';

  interface SpecMeta { repo: string; hasGraphql: boolean; files?: { full?: boolean | null } | null; }
  let specs = $state<SpecMeta[]>([]);

  function findSpec(name: string): SpecMeta | undefined {
    const stripped = name.replace(/\.(com|net|io|org|app)$/, '');
    return specs.find(s => s.repo === name || s.repo === stripped || name.startsWith(s.repo + '.'));
  }
  function specLabel(spec: SpecMeta): string {
    if (spec.hasGraphql && spec.files?.full) return 'REST+GQL';
    if (spec.hasGraphql) return 'GQL';
    return 'REST';
  }

  let { ondetailopen }: { ondetailopen: () => void } = $props();

  const SERVER  = 'rebuy';
  const POLL_MS = 30_000;

  // ── Types ──────────────────────────────────────────────────────────────────

  interface Param { key: string; label: string; type: 'text' | 'select'; options?: string[]; required?: boolean; default?: string; }
  interface Action { label: string; tool: string; params?: Param[]; dangerous?: boolean; primary?: boolean; }
  interface ServiceDef {
    id: string;
    label: string;
    statusTool: string;
    actions: Action[];
    linkedSuffix?: string;   // project name suffix match (e.g. '-db' matches 'rebuy-db')
    linkedNames?: string[];  // explicit project name list (for multi-project services)
  }
  interface ServiceState { ok: boolean | null; output: string; badge: string | null; }

  // ── Service definitions ────────────────────────────────────────────────────

  const SERVICES: ServiceDef[] = [
    { id: 'db', label: 'DB', statusTool: 'rebuy_db_status', linkedSuffix: '-db', actions: [
      { label: 'Start',    tool: 'rebuy_db_up',       primary: true },
      { label: 'Stop',     tool: 'rebuy_db_down',     primary: true },
      { label: 'Migrate',  tool: 'rebuy_db_migrate'  },
      { label: 'Status',   tool: 'rebuy_db_status'   },
      { label: 'Health',   tool: 'rebuy_db_health'   },
      { label: 'Logs',     tool: 'rebuy_db_logs'     },
      { label: 'Current',  tool: 'rebuy_db_current'  },
      { label: 'List',     tool: 'rebuy_db_list'     },
      { label: 'Install',  tool: 'rebuy_db_install'  },
      { label: 'Download', tool: 'rebuy_db_download' },
      { label: 'Reset',    tool: 'rebuy_db_reset',   dangerous: true },
      { label: 'Switch',   tool: 'rebuy_db_switch',  dangerous: true, params: [{ key: 'profile', label: 'Profile', type: 'select', options: ['local','stage','prod','prod-primary'], required: true }] },
    ]},
    { id: 'env', label: 'Env', statusTool: 'rebuy_env_status',
      linkedNames: ['admin-api', 'admin-nextjs', 'apiv2', 'rebuyengine.com'],
      actions: [
      { label: 'Start',      tool: 'rebuy_env_start',      primary: true },
      { label: 'Stop',       tool: 'rebuy_env_stop',       primary: true },
      { label: 'Restart',    tool: 'rebuy_env_restart',    primary: true },
      { label: 'Status',     tool: 'rebuy_env_status'     },
      { label: 'Logs',       tool: 'rebuy_env_logs'       },
      { label: 'Current',    tool: 'rebuy_env_current'    },
      { label: 'History',    tool: 'rebuy_env_history'    },
      { label: 'Generate',   tool: 'rebuy_env_generate'   },
      { label: 'Validate',   tool: 'rebuy_env_validate'   },
      { label: 'Dev',        tool: 'rebuy_env_dev'        },
      { label: 'DNS Dev',    tool: 'rebuy_env_dns_dev'    },
      { label: 'DNS Prod',   tool: 'rebuy_env_dns_prod'   },
      { label: 'DNS Status', tool: 'rebuy_env_dns_status' },
    ]},
    { id: 'engines', label: 'Engines', statusTool: 'rebuy_engines_status', actions: [
      { label: 'Start',  tool: 'rebuy_engines_start',  primary: true },
      { label: 'Stop',   tool: 'rebuy_engines_stop',   primary: true },
      { label: 'Status', tool: 'rebuy_engines_status' },
      { label: 'List',   tool: 'rebuy_engines_list'   },
      { label: 'Switch', tool: 'rebuy_engines_switch', dangerous: true, params: [{ key: 'cluster', label: 'Cluster', type: 'select', options: ['staging','prod'], required: true }] },
    ]},
    { id: 'tunnel', label: 'Tunnel', statusTool: 'rebuy_tunnel_status', actions: [
      { label: 'Start',  tool: 'rebuy_tunnel_start',  primary: true },
      { label: 'Stop',   tool: 'rebuy_tunnel_stop',   primary: true },
      { label: 'Status', tool: 'rebuy_tunnel_status' },
      { label: 'Extend', tool: 'rebuy_tunnel_extend', params: [
        { key: 'profile', label: 'Profile', type: 'text', required: false },
        { key: 'minutes', label: 'Minutes', type: 'text', default: '60', required: false },
      ]},
    ]},
    { id: 'network', label: 'Network', statusTool: 'rebuy_network_status', actions: [
      { label: 'Status', tool: 'rebuy_network_status', primary: true },
      { label: 'Create', tool: 'rebuy_network_create' },
      { label: 'Remove', tool: 'rebuy_network_remove', dangerous: true },
    ]},
  ];

  // ── State ──────────────────────────────────────────────────────────────────

  let servicesOpen  = $state(false);
  let mode          = $state<string | null>(null);
  let dockerRunning = $state(false);
  let dockerEngine  = $state('unknown');
  let dockerStarting = $state(false);
  let serviceStates = $state<Record<string, ServiceState>>(
    Object.fromEntries(SERVICES.map((s) => [s.id, { ok: null, output: '', badge: null }]))
  );
  let openModal         = $state<string | null>(null);
  let modalOutput       = $state('');
  let modalOutputLabel  = $state('');
  let modalPending      = $state<Action | null>(null);
  let modalParams       = $state<Record<string, string>>({});
  let modalBusy         = $state(false);
  let modalPendingTool  = $state<string | null>(null);
  let linkedServices    = $state<import('../types/sidebar').DockerService[]>([]);
  let actionsOpen       = false; // kept for composeAct/containerAct compat; details handles visual toggle
  let svcDialog         = $state<HTMLDialogElement | null>(null);

  $effect(() => {
    if (!svcDialog) return;
    if (openModal) { if (!svcDialog.open) svcDialog.showModal(); }
    else { try { svcDialog.close(); } catch {} }
  });
  let logProject        = $state<Project | null>(null);

  // Per-service linked projects (empty array = none found)
  let linkedProjects = $state<Record<string, Project[]>>(
    Object.fromEntries(SERVICES.map(s => [s.id, []]))
  );

  // ── Linked project resolution ──────────────────────────────────────────────

  function resolveLinkedProjects() {
    const all = getAllProjects();
    const owned: string[] = [];
    const next: Record<string, Project[]> = {};
    for (const svc of SERVICES) {
      const matches: Project[] = [];
      if (svc.linkedSuffix) {
        const m = all.find(p => p.name.endsWith(svc.linkedSuffix!));
        if (m) { matches.push(m); owned.push(m.name); }
      }
      if (svc.linkedNames) {
        for (const name of svc.linkedNames) {
          const m = all.find(p => p.name === name);
          if (m && !matches.find(x => x.name === m.name)) { matches.push(m); owned.push(m.name); }
        }
      }
      next[svc.id] = matches;
    }
    linkedProjects = next;
    setLinkedProjectNames(owned);
  }

  // Re-resolve whenever the project list changes.
  $effect(() => { void getAllProjects(); resolveLinkedProjects(); });

  // Derive schema name for a suffix-linked project (e.g. "rebuy-db" + "-db" → "rebuy")
  function schemaForLinked(svc: ServiceDef): string | null {
    if (!svc.linkedSuffix) return null;
    const linked = linkedProjects[svc.id].find(p => p.name.endsWith(svc.linkedSuffix!));
    if (!linked) return null;
    return linked.name.slice(0, -svc.linkedSuffix.length) || null;
  }

  // The primary linked project for DockerComposePanel (suffix-linked, single compose file)
  function primaryLinked(svc: ServiceDef): Project | null {
    if (!svc.linkedSuffix) return null;
    return linkedProjects[svc.id].find(p => p.name.endsWith(svc.linkedSuffix!)) ?? null;
  }

  // ── Polling ────────────────────────────────────────────────────────────────

  async function pollAll() {
    await Promise.allSettled([
      pollMode(),
      pollDockerEngine(),
      ...SERVICES.map((s) => pollService(s)),
    ]);
  }

  async function pollMode() {
    try { mode = (await callTool(SERVER, 'rebuy_mode_current')).trim(); } catch {}
  }

  async function pollDockerEngine() {
    try {
      const res = await fetch('/api/docker/engine');
      if (res.ok) { const d = await res.json(); dockerRunning = d.running; dockerEngine = d.engine ?? 'unknown'; }
    } catch {}
  }

  async function pollService(svc: ServiceDef) {
    try {
      const output = await callTool(SERVER, svc.statusTool);
      serviceStates[svc.id] = { ok: parseStatus(output), output, badge: parseBadge(svc.id, output) };
    } catch {}
  }

  onMount(() => {
    servicesOpen = localStorage.getItem('sidebar-services-open') === '1';
    pollAll();
    listSpecs().then(s => { specs = s as SpecMeta[]; }).catch(() => {});
    const t = setInterval(pollAll, POLL_MS);
    return () => { clearInterval(t); };
  });

  function toggleServices() {
    servicesOpen = !servicesOpen;
    localStorage.setItem('sidebar-services-open', servicesOpen ? '1' : '0');
  }

  // ── Service modal actions ──────────────────────────────────────────────────

  function openServiceModal(id: string) {
    openModal = id; modalOutput = ''; modalOutputLabel = '';
    modalPending = null; linkedServices = []; actionsOpen = false;
    logProject = null;
    // auto-select first linked project
    const projs = linkedProjects[id] ?? [];
    if (projs.length > 0) selectProject(projs[0]);
  }

  async function selectProject(proj: Project) {
    logProject = proj; linkedServices = [];
    try {
      const res = await fetch(`/api/docker/services?path=${encodeURIComponent(proj.path)}`);
      const d   = await res.json();
      linkedServices = (d.services ?? []).sort((a: DockerService, b: DockerService) => a.name.localeCompare(b.name));
    } catch {}
  }

  async function composeAct(action: string) {
    if (!logProject) return;
    modalBusy = true; actionsOpen = false;
    try {
      const res = await fetch('/api/docker/action', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ projectPath: logProject.path, action }),
      });
      const d = await res.json();
      modalOutput = d.output ?? ''; modalOutputLabel = `${logProject.name}: ${action}`;
      if (action !== 'logs') setTimeout(() => logProject && selectProject(logProject), 1200);
    } catch (e: any) { modalOutput = `Error: ${e.message}`; modalOutputLabel = action; }
    finally { modalBusy = false; }
  }

  async function containerAct(svcName: string, action: string) {
    if (!logProject) return;
    modalBusy = true; actionsOpen = false;
    try {
      const res = await fetch('/api/docker/action', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ projectPath: logProject.path, action, service: svcName }),
      });
      const d = await res.json();
      modalOutput = d.output ?? ''; modalOutputLabel = `${svcName}: ${action}`;
    } catch (e: any) { modalOutput = `Error: ${e.message}`; modalOutputLabel = action; }
    finally { modalBusy = false; }
  }

  function initAction(action: Action) {
    const defaults: Record<string, string> = {};
    action.params?.forEach((p) => { if (p.default) defaults[p.key] = p.default; });
    if (action.params?.some((p) => p.required)) {
      modalParams = defaults; modalPending = action;
    } else {
      runAction(action, defaults);
    }
  }

  async function runAction(action: Action, args: Record<string, string>) {
    modalBusy = true; modalPendingTool = action.tool;
    try {
      const cleaned: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(args)) if (v !== '') cleaned[k] = v;
      const text = await callTool(SERVER, action.tool, cleaned);
      modalOutput = text; modalOutputLabel = action.label; modalPending = null;
      if (action.tool === openModal + '_status' || action.label === 'Status') {
        serviceStates[openModal!] = { ok: parseStatus(text), output: text, badge: parseBadge(openModal!, text) };
      } else {
        await new Promise((r) => setTimeout(r, 1200));
        try {
          const svc = SERVICES.find((s) => s.id === openModal);
          if (svc) { const st = await callTool(SERVER, svc.statusTool); serviceStates[svc.id] = { ok: parseStatus(st), output: st, badge: parseBadge(svc.id, st) }; }
        } catch {}
      }
    } catch (e: any) { modalOutput = `Error: ${e.message ?? 'unknown'}`; }
    finally { modalBusy = false; modalPendingTool = null; }
  }

  async function startDocker() {
    dockerStarting = true;
    try {
      const res = await fetch('/api/docker/engine/start', { method: 'POST' });
      const d   = await res.json();
      if (!res.ok) throw new Error(d.error ?? `HTTP ${res.status}`);
      setTimeout(pollDockerEngine, 2000);
    } catch {} finally { dockerStarting = false; }
  }

  const activeSvc   = $derived(openModal ? SERVICES.find((s) => s.id === openModal) : null);
  const activeState = $derived(openModal ? serviceStates[openModal] : null);
</script>

<!-- ── Services panel ──────────────────────────────────────────────────────── -->
<div class="sp-panel">
  <div class="sp-header">
    <button class="sp-projects-toggle" onclick={toggleServices}>
      <span class="tree-arrow">{servicesOpen ? '▾' : '▸'}</span>
      <span class="sp-title">Services</span>
    </button>
    {#if mode}
      <span class="sp-badge sp-badge-{badgeTier(mode)}">{mode}</span>
    {/if}
    <button class="sp-detail-btn" onclick={ondetailopen}>Details ↗</button>
  </div>

  {#if servicesOpen}
    <!-- Docker engine row -->
    <button class="sp-row sp-row-btn" onclick={dockerRunning ? undefined : startDocker}
            disabled={dockerStarting}
            title={dockerRunning ? `docker (${dockerEngine})` : 'Click to start Docker'}>
      <StatusDot ok={dockerRunning} />
      <span class="sp-label">docker ({dockerEngine})</span>
    </button>

    <!-- Service rows + linked project sub-rows -->
    {#each SERVICES as svc (svc.id)}
      {@const state  = serviceStates[svc.id]}
      {@const linked = linkedProjects[svc.id]}
      <button class="sp-row sp-row-btn" onclick={() => openServiceModal(svc.id)}
              title={stripAnsi(state.output) || undefined}>
        <StatusDot ok={state.ok} />
        <span class="sp-label-btn">{svc.label}</span>
        {#if state.badge}
          <span class="sp-badge sp-badge-{badgeTier(state.badge)}">{state.badge}</span>
        {/if}
      </button>
      {#each linkedProjects[svc.id] as proj (proj.name)}
        {@const schema  = svc.linkedSuffix && proj.name.endsWith(svc.linkedSuffix)
          ? proj.name.slice(0, -svc.linkedSuffix.length) || null : null}
        {@const projSpec = findSpec(proj.name)}
        <div class="sp-row-item sp-row-sub-item">
          <button class="sp-row sp-row-btn sp-row-sub" onclick={() => openServiceModal(svc.id)}
                  title="{proj.name} — {proj.path}">
            <span class="sp-sub-indent"></span>
            <StatusDot ok={proj.running} />
            <span class="sp-label-btn sp-label-sub">{proj.name}</span>
          </button>
          {#if projSpec}
            <a class="sp-badge sp-badge-{projSpec.hasGraphql && !projSpec.files?.full ? 'gql' : 'rest'}"
               href="/api-docs/{projSpec.repo}" title="Open {specLabel(projSpec)} docs">
              {specLabel(projSpec)}
            </a>
          {/if}
          {#if schema}
            <a class="sp-badge sp-badge-db" href="/schema?db={encodeURIComponent(schema)}"
               title="Open schema: {schema}">DB</a>
          {/if}
        </div>
      {/each}
    {/each}
  {/if}
</div>

<!-- ── Service modal — rendered directly (no snippet) so all $state is reactive ── -->
{#if activeSvc && activeState}
  {@const allLinked = linkedProjects[activeSvc.id]}
  {@const modalSchema = schemaForLinked(activeSvc)}
  <dialog bind:this={svcDialog} class="svc-dialog"
          onclose={() => { openModal = null; actionsOpen = false; }}
          onclick={(e) => { if (e.target === svcDialog) { openModal = null; actionsOpen = false; } }}>
    <div class="svc-modal-inner">
      <!-- header -->
      <div class="svc-modal-header">
        <h3>{activeSvc.label}</h3>
        <button class="modal-close" onclick={() => { openModal = null; actionsOpen = false; }}>✕</button>
      </div>
      <!-- body -->
      <div class="svc-modal-body">
        <!-- Row 1: status + source + schema link -->
        <div class="svc-head">
          <StatusDot ok={activeState.ok} />
          <span class="svc-src">rebuy CLI</span>
          {#if modalSchema}
            <a class="svc-schema-link" href="/schema?db={encodeURIComponent(modalSchema)}"
               onclick={() => { openModal = null; }}>schema ↗</a>
          {/if}
        </div>

        <!-- Native select — multi-level via optgroup, zero JS reactivity needed -->
        <select class="ctrl-select" disabled={modalBusy}
                onchange={(e) => {
                  const sel = e.currentTarget as HTMLSelectElement;
                  const val = sel.value;
                  sel.value = '';
                  if (!val) return;
                  if (val.startsWith('compose:')) { composeAct(val.slice(8)); }
                  else if (val.startsWith('cnt:')) {
                    const idx = val.indexOf(':', 4);
                    containerAct(val.slice(4, idx), val.slice(idx + 1));
                  } else {
                    const action = activeSvc?.actions.find(a => a.tool === val);
                    if (action) initAction(action);
                  }
                }}>
          <option value="">☰ Controls…</option>
          <optgroup label="{activeSvc.label}">
            {#each activeSvc.actions as action (action.tool)}
              <option value={action.tool}>{action.label}</option>
            {/each}
          </optgroup>
          {#if logProject}
            <optgroup label="{logProject.name} — compose">
              <option value="compose:up">Up</option>
              <option value="compose:down">Down</option>
              <option value="compose:restart">Restart</option>
              <option value="compose:pull">Pull</option>
            </optgroup>
            {#each linkedServices as csvc (csvc.name)}
              <optgroup label="{csvc.name}">
                <option value="cnt:{csvc.name}:up">Up</option>
                <option value="cnt:{csvc.name}:stop">Stop</option>
                <option value="cnt:{csvc.name}:restart">Restart</option>
                <option value="cnt:{csvc.name}:logs">Logs</option>
              </optgroup>
            {/each}
          {/if}
        </select>

        <!-- Param form -->
        {#if modalPending}
          <div class="param-form">
            <span class="param-title">{modalPending.label}</span>
            {#each modalPending.params ?? [] as p (p.key)}
              <div class="field">
                <label for="sp-{p.key}">{p.label}{p.required ? ' *' : ''}</label>
                {#if p.type === 'select'}
                  <select id="sp-{p.key}" bind:value={modalParams[p.key]}>
                    <option value="">— choose —</option>
                    {#each p.options ?? [] as opt (opt)}<option value={opt}>{opt}</option>{/each}
                  </select>
                {:else}
                  <input id="sp-{p.key}" type="text" placeholder={p.default ?? ''} bind:value={modalParams[p.key]} />
                {/if}
              </div>
            {/each}
            <div class="param-btns">
              <button class="cbtn cbtn-primary"
                      disabled={modalBusy || (modalPending.params?.some((p) => p.required && !modalParams[p.key]) ?? false)}
                      onclick={() => modalPending && runAction(modalPending, modalParams)}>Run</button>
              <button class="cbtn" onclick={() => modalPending = null}>Cancel</button>
            </div>
          </div>
        {/if}

        <!-- Project chips -->
        {#if allLinked.length > 1}
          <div class="proj-chips">
            {#each allLinked as proj (proj.name)}
              <button class="proj-chip" class:active={logProject?.name === proj.name}
                      onclick={() => selectProject(proj)}>
                <StatusDot ok={proj.running} />{proj.name}
              </button>
            {/each}
          </div>
        {/if}

        <!-- Terminal hero -->
        <LogViewer projectPath={logProject?.path ?? ''}
                   services={linkedServices}
                   actionOutput={modalOutput}
                   actionLabel={modalOutputLabel}
                   busy={modalBusy} />
      </div>
    </div>
  </dialog>
{/if}

<style>
  .sp-panel { padding: var(--space-2) 0; border-bottom: 1px solid var(--color-border); }
  .sp-panel:last-child { border-bottom: none; }

  .sp-header { display: flex; align-items: center; gap: var(--space-2); padding: var(--space-1) var(--space-3); }
  .sp-title { font-size: var(--text-xs); font-weight: var(--weight-semibold); color: var(--color-text-dim); text-transform: uppercase; letter-spacing: 0.06em; flex: 1; }

  .sp-badge { font-size: var(--text-xs); font-weight: var(--weight-semibold); padding: 1px 5px; border-radius: var(--radius-sm); text-transform: uppercase; }
  .sp-badge-danger { background: rgba(248,113,113,0.15); color: var(--color-error); }
  .sp-badge-warn   { background: rgba(250,204, 21,0.15); color: var(--color-warning); }
  .sp-badge-dim    { background: var(--color-surface-2); color: var(--color-text-dim); }

  .sp-detail-btn {
    background: none; border: none; cursor: pointer; font-size: var(--text-xs);
    color: var(--color-text-dim); padding: 2px var(--space-1); border-radius: var(--radius-sm);
    transition: color var(--transition-fast);
  }
  .sp-detail-btn:hover { color: var(--color-accent); }

  .sp-row { display: flex; align-items: center; gap: var(--space-2); padding: var(--space-1) var(--space-3); font-size: var(--text-sm); }
  .sp-row-btn { width: 100%; background: none; border: none; cursor: pointer; text-align: left; border-radius: var(--radius-sm); transition: background var(--transition-fast); }
  .sp-row-btn:hover { background: var(--color-surface-2); }
  .sp-row-btn:disabled { cursor: default; }
  .sp-row-sub { padding-left: calc(var(--space-3) + 14px); opacity: 0.8; }

  .sp-label     { color: var(--color-text-muted); flex: 1; }
  .sp-label-btn { font-size: var(--text-sm); color: var(--color-text-muted); flex: 1; text-align: left; }
  .sp-label-sub { font-size: var(--text-xs); }
  .sp-sub-indent { width: 6px; flex-shrink: 0; }

  .sp-projects-toggle { background: none; border: none; cursor: pointer; display: flex; align-items: center; gap: var(--space-1); flex: 1; padding: 0; }
  .tree-arrow { font-size: var(--text-xs); color: var(--color-text-dim); }

  /* sub-row with schema badge */
  .sp-row-sub-item { display: flex; align-items: center; gap: 0; padding: 0 var(--space-3) 0 0; }
  .sp-badge { font-size: 9px; font-weight: var(--weight-semibold); letter-spacing: 0.04em; padding: 1px 5px; border-radius: var(--radius-sm); line-height: 1.4; flex-shrink: 0; text-decoration: none; transition: opacity var(--transition-fast); }
  .sp-badge:hover { opacity: 0.75; }
  .sp-badge-db { background: rgba(52, 211, 153, 0.18); color: #34d399; }

  /* schema link in modal */
  .svc-schema-link { font-size: var(--text-xs); color: var(--color-accent); text-decoration: none; }
  .svc-schema-link:hover { text-decoration: underline; }

  /* ── Modal ── */

  /* overlay for closing ⋯ dropdown */
  .dd-overlay { position: fixed; inset: 0; z-index: 10; }

  .dd-wrap { position: relative; }
  .dd-menu {
    position: absolute; right: 0; top: calc(100% + 4px); z-index: 11;
    background: var(--color-surface); border: 1px solid var(--color-border);
    border-radius: var(--radius-md); box-shadow: var(--shadow-md);
    min-width: 130px; padding: 4px;
    display: flex; flex-direction: column; gap: 1px;
  }
  .dd-menu-left { right: auto; left: 0; }
  .dd-item {
    background: none; border: none; cursor: pointer; text-align: left;
    font-size: var(--text-xs); color: var(--color-text-muted); font-family: var(--font-sans);
    padding: 6px 12px; border-radius: var(--radius-sm);
    transition: background var(--transition-fast), color var(--transition-fast);
    white-space: nowrap;
  }
  .dd-item:hover:not(:disabled) { background: var(--color-surface-2); color: var(--color-text); }
  .dd-item:disabled { opacity: 0.38; cursor: not-allowed; }
  .dd-item-danger   { color: var(--color-error); }
  .dd-item-danger:hover:not(:disabled) { background: rgba(248,113,113,0.1); }
  .dd-item-primary  { color: var(--color-accent); }
  .dd-item-primary:hover:not(:disabled) { background: rgba(124,106,247,0.1); }
  .dd-sep { height: 1px; background: var(--color-border); margin: 3px 0; }
  .dd-menu-scrollable { max-height: 60vh; overflow-y: auto; }
  .dd-group { font-size: 9px; font-weight: var(--weight-semibold); text-transform: uppercase; letter-spacing: 0.06em; color: var(--color-text-dim); padding: 6px 12px 2px; }
  .dd-group-container { display: flex; align-items: center; gap: 5px; }

  /* overlay + fixed dropdown */
  .dd-menu-fixed {
    position: fixed;
    z-index: 9999;
  }

  /* compact service header row */
  .svc-head { display: flex; align-items: center; gap: var(--space-2); flex-wrap: wrap; margin-bottom: var(--space-2); flex-shrink: 0; }
  .svc-src   { font-size: var(--text-xs); color: var(--color-text-faint); font-family: var(--font-mono); flex: 1; }

  .svc-menu-btn {
    display: inline-flex; align-items: center; justify-content: center;
    background: var(--color-surface-2); border: 1px solid var(--color-border);
    border-radius: var(--radius-sm); cursor: pointer;
    font-size: 18px; line-height: 1; color: var(--color-text-muted);
    padding: 4px 10px;
    transition: background var(--transition-fast), color var(--transition-fast);
  }
  .svc-menu-btn:hover { background: var(--color-surface-3, #2c2c2c); color: var(--color-text); }

  /* project chip tabs */
  .proj-chips { display: flex; flex-wrap: wrap; gap: 4px; margin-bottom: var(--space-2); flex-shrink: 0; }
  .proj-chip {
    display: flex; align-items: center; gap: 4px;
    background: var(--color-surface-2); border: 1px solid var(--color-border);
    border-radius: var(--radius-sm); cursor: pointer;
    font-size: var(--text-xs); color: var(--color-text-muted); font-family: var(--font-sans);
    padding: 3px 8px; white-space: nowrap;
    transition: border-color var(--transition-fast), color var(--transition-fast);
  }
  .proj-chip:hover { color: var(--color-text); border-color: var(--color-accent); }
  .proj-chip.active { border-color: var(--color-accent); color: var(--color-accent); background: rgba(124,106,247,0.1); }

  /* ── Direct dialog (bypasses snippet boundary) ── */
  .svc-dialog {
    background: transparent; border: none; padding: 0;
    max-width: 100vw; max-height: 100vh; overflow: visible;
    margin: auto;
  }
  .svc-dialog::backdrop { background: rgba(0,0,0,0.65); }
  .svc-modal-inner {
    background: var(--color-surface); border: 1px solid var(--color-border);
    border-radius: var(--radius-lg); color: var(--color-text);
    padding: var(--space-6); width: min(1000px, 92vw);
    box-shadow: var(--shadow-lg); max-height: 88vh;
    display: flex; flex-direction: column;
  }
  .svc-modal-header {
    display: flex; align-items: center; justify-content: space-between;
    margin-bottom: var(--space-4); flex-shrink: 0;
  }
  .svc-modal-header h3 { margin: 0; font-size: var(--text-lg); }
  .modal-close {
    background: none; border: none; color: var(--color-text-dim);
    cursor: pointer; font-size: var(--text-base); padding: var(--space-1);
  }
  .svc-modal-body {
    flex: 1; min-height: 0; display: flex; flex-direction: column;
  }

  /* spec badges */
  .sp-badge-rest { background: rgba(124,106,247,0.18); color: #a78bfa; }
  .sp-badge-gql  { background: rgba(231, 70, 160, 0.18); color: #e746a0; }

  /* controls select */
  .ctrl-select {
    flex-shrink: 0; margin-bottom: var(--space-2);
    background: var(--color-surface-2); border: 1px solid var(--color-border);
    border-radius: var(--radius-sm); color: var(--color-text-muted);
    font-size: var(--text-sm); font-family: var(--font-sans);
    padding: 4px 8px; cursor: pointer; outline: none;
  }
  .ctrl-select:hover { border-color: var(--color-accent); color: var(--color-text); }
  .ctrl-select:disabled { opacity: 0.5; cursor: not-allowed; }

  /* inline action panel */
  .action-panel { border-top: 1px solid var(--color-border); border-bottom: 1px solid var(--color-border); padding: var(--space-2) 0; margin-bottom: var(--space-2); flex-shrink: 0; display: flex; flex-direction: column; gap: var(--space-1); }
  .ap-group-label { font-size: 9px; font-weight: var(--weight-semibold); text-transform: uppercase; letter-spacing: 0.06em; color: var(--color-text-dim); padding: 4px 0 2px; }
  .ap-container-label { display: flex; align-items: center; gap: 5px; }
  .ap-row { display: flex; flex-wrap: wrap; gap: var(--space-1); }
  .ap-btn {
    background: var(--color-surface-2); border: 1px solid var(--color-border);
    border-radius: var(--radius-sm); cursor: pointer; font-size: var(--text-xs);
    color: var(--color-text-muted); font-family: var(--font-sans);
    padding: 3px 10px; white-space: nowrap;
    transition: background var(--transition-fast), color var(--transition-fast);
  }
  .ap-btn:hover:not(:disabled) { background: var(--color-surface-3, #2c2c2c); color: var(--color-text); }
  .ap-btn:disabled { opacity: 0.38; cursor: not-allowed; }
  .ap-btn-primary { border-color: rgba(124,106,247,0.4); color: var(--color-accent); background: rgba(124,106,247,0.1); }
  .ap-btn-primary:hover:not(:disabled) { background: rgba(124,106,247,0.2); }
  .ap-btn-danger  { border-color: rgba(248,113,113,0.35); color: var(--color-error); background: rgba(248,113,113,0.08); }
  .ap-btn-danger:hover:not(:disabled) { background: rgba(248,113,113,0.15); }

  /* param form */
  .param-form { margin-bottom: var(--space-2); padding: var(--space-2); background: var(--color-surface-2); border-radius: var(--radius-md); display: flex; flex-wrap: wrap; align-items: flex-end; gap: var(--space-2); flex-shrink: 0; }
  .param-title { font-size: var(--text-xs); font-weight: var(--weight-semibold); color: var(--color-text-dim); width: 100%; }
  .param-btns { display: flex; gap: var(--space-1); }
  .field { display: flex; flex-direction: column; gap: 3px; }
  .field label { font-size: var(--text-xs); color: var(--color-text-dim); }
  .field input, .field select { background: var(--color-bg); border: 1px solid var(--color-border); border-radius: var(--radius-sm); color: var(--color-text); font-size: var(--text-sm); padding: 4px 8px; outline: none; }
  .field input:focus, .field select:focus { border-color: var(--color-accent); }
</style>
