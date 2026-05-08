<script lang="ts">
  import { onMount } from 'svelte';
  import StatusDot from './StatusDot.svelte';
  import Modal from './Modal.svelte';
  import DockerComposePanel from './DockerComposePanel.svelte';
  import LogViewer from './LogViewer.svelte';
  import { callTool, stripAnsi } from '../mcp';
  import { listSpecs, listSchemaDatabases } from '../api/client';
  import {
    setLinkedSpecRepos, setLinkedSchemas,
    setAllProjects, getLinkedProjectNames,
  } from '../stores/sidebar.svelte';
  import type { Project, DockerService } from '../types/sidebar';

  const SERVER  = 'rebuy';
  const POLL_MS = 30_000;
  const HOME    = '/Users/scottkey';

  interface SpecMeta { repo: string; hasGraphql: boolean; files?: { full?: boolean | null } | null; }
  interface DbInfo   { name: string; [k: string]: any }

  // ── State ──────────────────────────────────────────────────────────────────

  let projectsOpen = $state(false);
  let projects     = $state<Project[]>([]);
  let projectsBusy = $state(false);
  let specs        = $state<SpecMeta[]>([]);
  let schemas      = $state<DbInfo[]>([]);

  // ── Spec / schema matching ─────────────────────────────────────────────────

  // "rebuyengine.com" → "rebuyengine"
  function findSpec(name: string): SpecMeta | undefined {
    const stripped = name.replace(/\.(com|net|io|org|app)$/, '');
    return specs.find(s => s.repo === name || s.repo === stripped || name.startsWith(s.repo + '.'));
  }

  // "rebuy-db" → schema "rebuy" (strip -db suffix)
  function findSchema(name: string): DbInfo | undefined {
    const base = name.replace(/-db$/i, '');
    if (base === name) return undefined;
    return schemas.find(db => db.name === base);
  }

  function specLabel(spec: SpecMeta): string {
    if (spec.hasGraphql && spec.files?.full) return 'REST+GQL';
    if (spec.hasGraphql) return 'GQL';
    return 'REST';
  }

  // ── Project modal state ────────────────────────────────────────────────────

  let openProject     = $state<Project | null>(null);
  let projOutput      = $state('');
  let projOutputLabel = $state('');
  let projBusy        = $state(false);
  let projBusyTool    = $state<string | null>(null);
  let projDockerSvcs  = $state<DockerService[]>([]);
  let projActionsOpen = $state(false);

  // ── Polling ────────────────────────────────────────────────────────────────

  async function pollProjects() {
    projectsBusy = true;
    try {
      const output = await callTool(SERVER, 'rebuy_project_list', { all: true });
      const parsed = parseProjectList(output);
      const checks = parsed.map(async (proj) => {
        try {
          const res = await fetch(`/api/docker/services?path=${encodeURIComponent(proj.path)}`);
          const d   = await res.json();
          return { ...proj, running: (d.services ?? []).some((s: DockerService) => s.running) };
        } catch { return proj; }
      });
      projects = await Promise.all(checks);
      setAllProjects(projects);
    } catch {} finally { projectsBusy = false; }
  }

  function parseProjectList(output: string): Project[] {
    const result: Project[] = [];
    let current: Partial<Project> | null = null;
    for (const line of output.split('\n')) {
      const clean = line.replace(/\x1b\[[0-9;]*m/g, '').trim();
      const nameMatch = clean.match(/^📦\s+(.+)$/);
      if (nameMatch) {
        if (current?.name && current?.path) result.push({ running: false, ...current } as Project);
        current = { name: nameMatch[1].trim() }; continue;
      }
      const pathMatch = clean.match(/^Path:\s+(.+)$/);
      if (pathMatch && current) current.path = pathMatch[1].trim();
    }
    if (current?.name && current?.path) result.push({ running: false, ...current } as Project);
    return result;
  }

  onMount(() => {
    projectsOpen = localStorage.getItem('sidebar-projects-open') === '1';
    pollProjects();
    listSpecs().then(s => { specs = s as unknown as SpecMeta[]; }).catch(() => {});
    listSchemaDatabases().then(s => { schemas = s as DbInfo[]; }).catch(() => {});
    const t = setInterval(pollProjects, POLL_MS);
    return () => { clearInterval(t); };
  });

  function toggleProjects() {
    projectsOpen = !projectsOpen;
    localStorage.setItem('sidebar-projects-open', projectsOpen ? '1' : '0');
  }

  // ── Project MCP actions (rebuy CLI level) ──────────────────────────────────

  async function runProjectAction(tool: string, label: string) {
    if (!openProject) return;
    projBusy = true; projBusyTool = tool; projOutputLabel = label;
    try {
      projOutput = await callTool(SERVER, tool, { path: openProject.path });
    } catch (e: any) { projOutput = `Error: ${e.message}`; }
    finally { projBusy = false; projBusyTool = null; }
  }

  // ── Sync linked stores ─────────────────────────────────────────────────────

  $effect(() => {
    const linkedSpecs = specs
      .filter(sp => projects.some(p => findSpec(p.name)?.repo === sp.repo))
      .map(sp => sp.repo);
    setLinkedSpecRepos(linkedSpecs);

    const linkedSchemaNames = schemas
      .filter(db => projects.some(p => findSchema(p.name)?.name === db.name))
      .map(db => db.name);
    setLinkedSchemas(linkedSchemaNames);
  });

  // Visible projects: filter out those owned by a service (ServicesPanel writes this).
  let visibleProjects = $derived(
    projects.filter(p => !getLinkedProjectNames().includes(p.name))
  );
</script>

<!-- ── Sidebar section ─────────────────────────────────────────────────────── -->
<div class="sp-panel">
  <div class="sp-header">
    <button class="sp-projects-toggle" onclick={toggleProjects}>
      <span class="tree-arrow">{projectsOpen ? '▾' : '▸'}</span>
      <span class="sp-title">Projects</span>
    </button>
    <button class="sp-detail-btn" onclick={pollProjects} disabled={projectsBusy}>
      {projectsBusy ? '…' : '↺'}
    </button>
  </div>

  {#if projectsOpen}
    <div class="sp-projects-list">
      {#if visibleProjects.length === 0 && !projectsBusy}
        <div class="sp-empty">No projects found</div>
      {/if}
      {#each visibleProjects as proj (proj.name)}
        {@const projSpec   = findSpec(proj.name)}
        {@const projSchema = findSchema(proj.name)}
        <div class="sp-row-item">
          <button class="sp-row-btn" title={proj.path}
                  onclick={() => { openProject = proj; projOutput = ''; projOutputLabel = ''; }}>
            <StatusDot ok={proj.running} />
            <span class="sp-label-btn">{proj.name}</span>
          </button>
          {#if projSpec}
            <a class="sp-badge sp-badge-{projSpec.hasGraphql && !projSpec.files?.full ? 'gql' : 'rest'}"
               href="/api-docs/{projSpec.repo}" title="Open {specLabel(projSpec)} docs">
              {specLabel(projSpec)}
            </a>
          {/if}
          {#if projSchema}
            <a class="sp-badge sp-badge-db" href="/schema?db={encodeURIComponent(projSchema.name)}"
               title="Open schema: {projSchema.name}">DB</a>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<!-- ── Project modal ───────────────────────────────────────────────────────── -->
{#if openProject}
  <Modal open={!!openProject} onclose={() => { openProject = null; projActionsOpen = false; }} size="full"
         title="{openProject.name} — {openProject.path.replace(HOME + '/', '~/')}">
    {#snippet children()}
      {@const proj        = openProject!}
      {@const modalSpec   = findSpec(proj.name)}
      {@const modalSchema = findSchema(proj.name)}

      {#if projActionsOpen}
        <div class="dd-overlay" role="presentation" onclick={() => projActionsOpen = false} onkeydown={() => projActionsOpen = false}></div>
      {/if}

      <!-- Compact header: status + label + ⋯ menu (all actions) + links -->
      <div class="proj-head">
        <StatusDot ok={proj.running} />
        <span class="proj-title">Project</span>
        <span class="proj-src">rebuy CLI</span>

        <!-- All actions in ⋯ dropdown -->
        <div class="dd-wrap">
          <button class="cbtn cbtn-icon" title="Actions" onclick={() => projActionsOpen = !projActionsOpen}>⋯</button>
          {#if projActionsOpen}
            <div class="dd-menu">
              {#each [
                { label: 'Up All',  tool: 'rebuy_project_start',  primary: true  },
                { label: 'Down',    tool: 'rebuy_project_stop',   primary: true  },
                { label: 'Restart', tool: 'rebuy_project_restart', primary: false },
                { label: 'Status',  tool: 'rebuy_project_status',  primary: false },
                { label: 'Logs',    tool: 'rebuy_project_logs',    primary: false },
              ] as a (a.tool)}
                <button class="dd-item {a.primary ? 'dd-item-primary' : ''}" disabled={projBusy}
                        onclick={() => { projActionsOpen = false; runProjectAction(a.tool, a.label); }}>
                  {a.label}
                </button>
              {/each}
            </div>
          {/if}
        </div>

        <!-- Spec + schema links -->
        {#if modalSpec}
          <a class="proj-link" href="/api-docs/{modalSpec.repo}" onclick={() => openProject = null}>
            {specLabel(modalSpec)} docs ↗
          </a>
        {/if}
        {#if modalSchema}
          <a class="proj-link" href="/schema?db={encodeURIComponent(modalSchema.name)}" onclick={() => openProject = null}>
            schema ↗
          </a>
        {/if}
      </div>

      <!-- Compose + containers -->
      <div class="compose-wrap">
        <DockerComposePanel projectPath={proj.path} busy={projBusy}
                            onservicesloaded={(svcs) => { projDockerSvcs = svcs; }}
                            onoutput={(out, lbl) => { projOutput = out; projOutputLabel = lbl; }} />
      </div>

      <!-- Terminal — hero, fills remaining space -->
      <LogViewer projectPath={proj.path}
                 services={projDockerSvcs}
                 actionOutput={projOutput}
                 actionLabel={projOutputLabel}
                 busy={projBusy} />
    {/snippet}
  </Modal>
{/if}

<style>
  .sp-panel { padding: var(--space-2) 0; border-bottom: 1px solid var(--color-border); }
  .sp-panel:last-child { border-bottom: none; }

  .sp-header { display: flex; align-items: center; gap: var(--space-2); padding: var(--space-1) var(--space-3); }
  .sp-title { font-size: var(--text-xs); font-weight: var(--weight-semibold); color: var(--color-text-dim); text-transform: uppercase; letter-spacing: 0.06em; flex: 1; }

  .sp-detail-btn {
    background: none; border: none; cursor: pointer; font-size: var(--text-xs);
    color: var(--color-text-dim); padding: 2px var(--space-1); border-radius: var(--radius-sm);
    transition: color var(--transition-fast);
  }
  .sp-detail-btn:hover { color: var(--color-accent); }

  .sp-projects-toggle { background: none; border: none; cursor: pointer; display: flex; align-items: center; gap: var(--space-1); flex: 1; padding: 0; }
  .tree-arrow { font-size: var(--text-xs); color: var(--color-text-dim); }
  .sp-projects-list { padding: 0 0 var(--space-1); }
  .sp-empty { padding: var(--space-1) var(--space-3); font-size: var(--text-xs); color: var(--color-text-faint); }

  .sp-row-item { display: flex; align-items: center; gap: 0; padding: 0 var(--space-3) 0 0; }
  .sp-row-btn {
    flex: 1; min-width: 0; display: flex; align-items: center; gap: var(--space-2);
    padding: var(--space-1) var(--space-2) var(--space-1) var(--space-3);
    background: none; border: none; cursor: pointer; text-align: left;
    border-radius: var(--radius-sm); transition: background var(--transition-fast);
  }
  .sp-row-btn:hover { background: var(--color-surface-2); }
  .sp-label-btn { font-size: var(--text-sm); color: var(--color-text-muted); flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

  .sp-badge {
    font-size: 9px; font-weight: var(--weight-semibold); letter-spacing: 0.04em;
    padding: 1px 5px; border-radius: var(--radius-sm); line-height: 1.4; flex-shrink: 0;
    text-decoration: none; transition: opacity var(--transition-fast);
  }
  .sp-badge:hover { opacity: 0.75; }
  .sp-badge-rest { background: rgba(124,106,247,0.18); color: #a78bfa; }
  .sp-badge-gql  { background: rgba(231, 70, 160, 0.18); color: #e746a0; }
  .sp-badge-db   { background: rgba(52, 211, 153, 0.18); color: #34d399; }

  /* modal */
  .dd-overlay { position: fixed; inset: 0; z-index: 10; }
  .dd-wrap { position: relative; }
  .dd-menu {
    position: absolute; left: 0; top: calc(100% + 4px); z-index: 11;
    background: var(--color-surface); border: 1px solid var(--color-border);
    border-radius: var(--radius-md); box-shadow: var(--shadow-md);
    min-width: 130px; padding: 4px;
    display: flex; flex-direction: column; gap: 1px;
  }
  .dd-item {
    background: none; border: none; cursor: pointer; text-align: left;
    font-size: var(--text-xs); color: var(--color-text-muted); font-family: var(--font-sans);
    padding: 6px 12px; border-radius: var(--radius-sm);
    transition: background var(--transition-fast), color var(--transition-fast);
    white-space: nowrap;
  }
  .dd-item:hover:not(:disabled) { background: var(--color-surface-2); color: var(--color-text); }
  .dd-item:disabled { opacity: 0.38; cursor: not-allowed; }
  .dd-item-primary { color: var(--color-accent); }
  .dd-item-primary:hover:not(:disabled) { background: rgba(124,106,247,0.1); }

  .proj-head { display: flex; align-items: center; gap: var(--space-2); flex-wrap: wrap; margin-bottom: var(--space-2); flex-shrink: 0; }
  .proj-title { font-size: var(--text-sm); font-weight: var(--weight-semibold); color: var(--color-text); }
  .proj-src   { font-size: var(--text-xs); color: var(--color-text-faint); font-family: var(--font-mono); flex: 1; }
  .proj-link  { font-size: var(--text-xs); color: var(--color-accent); }
  .proj-link:hover { text-decoration: underline; }

  .compose-wrap { border-top: 1px solid var(--color-border); padding-top: var(--space-2); margin-bottom: var(--space-2); flex-shrink: 0; }
</style>
