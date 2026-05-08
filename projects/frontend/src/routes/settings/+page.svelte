<script lang="ts">
  import Button from '$lib/components/Button.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import DataTable from '$lib/components/DataTable.svelte';
  import { act } from '$lib/stores/notifications';
  import {
    listSpecs, registerSpec, unregisterSpec, refreshSpec,
    listSchemaDatabases, addSchemaDatabase, removeSchemaDatabase,
    listDockerRuntimes, addDockerRuntime, removeDockerRuntime,
    listMcpServers, addMcpServer, removeMcpServer,
  } from '$lib/api/client';
  import type { SpecMeta, SchemaDbInfo, DockerRuntimeInfo, McpServerInfo } from '$lib/api/types';

  type Tab = 'specs' | 'schema' | 'docker' | 'mcp';
  let tab = $state<Tab>('specs');

  // ── Specs ──────────────────────────────────────────────────────────────────
  let specs        = $state<SpecMeta[]>([]);
  let specsLoading = $state(false);
  let regRepo      = $state('');
  let regUrl       = $state('');
  let regBusy      = $state(false);

  async function loadSpecs() {
    specsLoading = true;
    specs = await act(() => listSpecs()) ?? specs;
    specsLoading = false;
  }

  async function doRegister() {
    if (!regRepo.trim() || !regUrl.trim()) return;
    regBusy = true;
    const result = await act(
      () => registerSpec({ body: { name: regRepo.trim(), url: regUrl.trim() } }),
      { success: `Registered ${regRepo}` },
    );
    if (result !== null) { regRepo = ''; regUrl = ''; await loadSpecs(); }
    regBusy = false;
  }

  async function doUnregister(name: string) {
    if (await act(() => unregisterSpec({ name }), { success: `Unregistered ${name}` })) await loadSpecs();
  }

  async function doRefresh(name: string) {
    if (await act(() => refreshSpec({ name }), { success: `Refreshed ${name}` })) await loadSpecs();
  }

  // ── Schema DBs ─────────────────────────────────────────────────────────────
  let dbs        = $state<SchemaDbInfo[]>([]);
  let dbsLoading = $state(false);
  let dbName     = $state('');
  let dbHost     = $state('localhost');
  let dbPort     = $state('3306');
  let dbDatabase = $state('');
  let dbUser     = $state('');
  let dbPass     = $state('');
  let dbAddBusy  = $state(false);

  async function loadDbs() {
    dbsLoading = true;
    dbs = await act(() => listSchemaDatabases()) ?? dbs;
    dbsLoading = false;
  }

  async function doAddDb() {
    if (!dbName.trim() || !dbDatabase.trim()) return;
    dbAddBusy = true;
    const result = await act(
      () => addSchemaDatabase({ body: {
        name: dbName.trim(), host: dbHost.trim() || 'localhost',
        port: Number(dbPort) || 3306, database: dbDatabase.trim(),
        username: dbUser.trim() || undefined, password: dbPass || undefined,
      } as any }),
      { success: `Added schema DB: ${dbName}` },
    );
    if (result !== null) { dbName = ''; dbDatabase = ''; dbUser = ''; dbPass = ''; await loadDbs(); }
    dbAddBusy = false;
  }

  async function doRemoveDb(name: string) {
    if (await act(() => removeSchemaDatabase({ name }), { success: `Removed ${name}` })) await loadDbs();
  }

  // ── Docker Runtimes ────────────────────────────────────────────────────────
  let runtimes        = $state<DockerRuntimeInfo[]>([]);
  let runtimesLoading = $state(false);
  let drName          = $state('');
  let drSocket        = $state('/var/run/docker.sock');
  let drAddBusy       = $state(false);

  async function loadRuntimes() {
    runtimesLoading = true;
    runtimes = await act(() => listDockerRuntimes()) ?? runtimes;
    runtimesLoading = false;
  }

  async function doAddRuntime() {
    if (!drName.trim()) return;
    drAddBusy = true;
    const result = await act(
      () => addDockerRuntime({ body: { name: drName.trim(), socketPath: drSocket.trim() || undefined } as any }),
      { success: `Added runtime: ${drName}` },
    );
    if (result !== null) { drName = ''; drSocket = '/var/run/docker.sock'; await loadRuntimes(); }
    drAddBusy = false;
  }

  async function doRemoveRuntime(name: string) {
    if (await act(() => removeDockerRuntime({ name }), { success: `Removed ${name}` })) await loadRuntimes();
  }

  // ── MCP Servers ────────────────────────────────────────────────────────────
  let mcpServers = $state<McpServerInfo[]>([]);
  let mcpLoading = $state(false);
  let mcpName    = $state('');
  let mcpCmd     = $state('');
  let mcpArgs    = $state('');
  let mcpEnv     = $state('');
  let mcpAddBusy = $state(false);

  async function loadMcp() {
    mcpLoading = true;
    mcpServers = await act(() => listMcpServers()) ?? mcpServers;
    mcpLoading = false;
  }

  async function doAddMcp() {
    if (!mcpName.trim() || !mcpCmd.trim()) return;
    mcpAddBusy = true;
    const args = mcpArgs.trim() ? mcpArgs.trim().split(/\s+/) : [];
    const env: Record<string, string> = {};
    if (mcpEnv.trim()) {
      for (const pair of mcpEnv.trim().split('\n')) {
        const [k, ...vs] = pair.split('=');
        if (k?.trim()) env[k.trim()] = vs.join('=').trim();
      }
    }
    const result = await act(
      () => addMcpServer({ body: { name: mcpName.trim(), command: mcpCmd.trim(), args, env } as any }),
      { success: `Added MCP server: ${mcpName}` },
    );
    if (result !== null) { mcpName = ''; mcpCmd = ''; mcpArgs = ''; mcpEnv = ''; await loadMcp(); }
    mcpAddBusy = false;
  }

  async function doRemoveMcp(name: string) {
    if (await act(() => removeMcpServer({ name }), { success: `Removed ${name}` })) await loadMcp();
  }

  $effect(() => {
    if (tab === 'specs')  loadSpecs();
    if (tab === 'schema') loadDbs();
    if (tab === 'docker') loadRuntimes();
    if (tab === 'mcp')    loadMcp();
  });
</script>

<svelte:head><title>Settings — orca</title></svelte:head>

<div class="page">
  <h1>Settings</h1>

  <div class="tabs">
    {#each [
      { id: 'specs',  label: 'API Specs' },
      { id: 'schema', label: 'Schema DBs' },
      { id: 'docker', label: 'Docker Runtimes' },
      { id: 'mcp',    label: 'MCP Servers' },
    ] as t (t.id)}
      <button class="tab" class:active={tab === t.id} onclick={() => tab = t.id as Tab}>{t.label}</button>
    {/each}
  </div>

  <!-- ── Specs tab ──────────────────────────────────────────────────────────── -->
  {#if tab === 'specs'}
    <section class="section">
      <h2 class="section-title">Register a new spec</h2>
      <div class="form-row">
        <input class="input" placeholder="Repo name (e.g. rebuyengine)" bind:value={regRepo} />
        <input class="input flex-1" placeholder="OpenAPI URL" bind:value={regUrl} />
        <Button onclick={doRegister} disabled={regBusy || !regRepo.trim() || !regUrl.trim()}>
          {regBusy ? 'Registering…' : 'Register'}
        </Button>
      </div>
    </section>

    <DataTable
      columns={[{label:'Repo'},{label:'Paths'},{label:'Base URL'},{label:'Captured'},{label:'Actions'}]}
      rows={specs}
      emptyText="No specs registered"
      loading={specsLoading}
    >
      {#snippet row(item)}
        {@const spec = item as SpecMeta}
        <td class="mono">{spec.repo}</td>
        <td>{spec.pathCount ?? '—'}</td>
        <td class="mono dim">{spec.baseUrl ?? '—'}</td>
        <td class="dim">{spec.capturedAt ? new Date(spec.capturedAt).toLocaleDateString() : '—'}</td>
        <td>
          <div class="row-actions">
            <Button size="sm" variant="ghost" onclick={() => doRefresh(spec.repo)}>Refresh</Button>
            <Button size="sm" variant="danger" onclick={() => doUnregister(spec.repo)}>Remove</Button>
          </div>
        </td>
      {/snippet}
    </DataTable>
  {/if}

  <!-- ── Schema DBs tab ─────────────────────────────────────────────────────── -->
  {#if tab === 'schema'}
    <section class="section">
      <h2 class="section-title">Add a schema database</h2>
      <div class="form-grid">
        <input class="input" placeholder="Name (e.g. rebuy-db)" bind:value={dbName} />
        <input class="input" placeholder="Host" bind:value={dbHost} />
        <input class="input" placeholder="Port" bind:value={dbPort} style="width:80px" />
        <input class="input" placeholder="Database name" bind:value={dbDatabase} />
        <input class="input" placeholder="Username (optional)" bind:value={dbUser} />
        <input class="input" type="password" placeholder="Password (optional)" bind:value={dbPass} />
      </div>
      <Button onclick={doAddDb} disabled={dbAddBusy || !dbName.trim() || !dbDatabase.trim()}>
        {dbAddBusy ? 'Adding…' : 'Add Database'}
      </Button>
    </section>

    <DataTable
      columns={[{label:'Name'},{label:'Host'},{label:'Database'},{label:'Status'},{label:'Actions'}]}
      rows={dbs}
      emptyText="No schema databases configured"
      loading={dbsLoading}
    >
      {#snippet row(item)}
        {@const db = item as SchemaDbInfo}
        <td class="mono">{db.name}</td>
        <td class="dim">{db.host ?? 'localhost'}</td>
        <td class="mono">{db.database}</td>
        <td><Badge color={db.enabled ? 'green' : 'gray'}>{db.enabled ? 'enabled' : 'disabled'}</Badge></td>
        <td>
          <div class="row-actions">
            <Button size="sm" variant="danger" onclick={() => doRemoveDb(db.name)}>Remove</Button>
          </div>
        </td>
      {/snippet}
    </DataTable>
  {/if}

  <!-- ── Docker Runtimes tab ────────────────────────────────────────────────── -->
  {#if tab === 'docker'}
    <section class="section">
      <h2 class="section-title">Add a Docker runtime</h2>
      <div class="form-row">
        <input class="input" placeholder="Name (e.g. local)" bind:value={drName} />
        <input class="input flex-1" placeholder="Socket path (e.g. /var/run/docker.sock)" bind:value={drSocket} />
        <Button onclick={doAddRuntime} disabled={drAddBusy || !drName.trim()}>
          {drAddBusy ? 'Adding…' : 'Add Runtime'}
        </Button>
      </div>
    </section>

    <DataTable
      columns={[{label:'Name'},{label:'Socket / Host'},{label:'Status'},{label:'Actions'}]}
      rows={runtimes}
      emptyText="No Docker runtimes configured"
      loading={runtimesLoading}
    >
      {#snippet row(item)}
        {@const rt = item as DockerRuntimeInfo}
        <td class="mono">{rt.name}</td>
        <td class="mono dim">{rt.socketPath ?? rt.host ?? rt.url ?? '—'}</td>
        <td><Badge color={rt.enabled ? 'green' : 'gray'}>{rt.enabled ? 'enabled' : 'disabled'}</Badge></td>
        <td>
          <div class="row-actions">
            <Button size="sm" variant="danger" onclick={() => doRemoveRuntime(rt.name)}>Remove</Button>
          </div>
        </td>
      {/snippet}
    </DataTable>
  {/if}

  <!-- ── MCP Servers tab ────────────────────────────────────────────────────── -->
  {#if tab === 'mcp'}
    <section class="section">
      <h2 class="section-title">Add an MCP server</h2>
      <div class="form-grid">
        <input class="input" placeholder="Name (e.g. my-server)" bind:value={mcpName} />
        <input class="input" placeholder="Command (e.g. npx)" bind:value={mcpCmd} />
        <input class="input" placeholder="Args (space-separated)" bind:value={mcpArgs} />
        <textarea class="input textarea" placeholder="Env vars (KEY=value, one per line)" bind:value={mcpEnv} rows="3"></textarea>
      </div>
      <Button onclick={doAddMcp} disabled={mcpAddBusy || !mcpName.trim() || !mcpCmd.trim()}>
        {mcpAddBusy ? 'Adding…' : 'Add MCP Server'}
      </Button>
    </section>

    <DataTable
      columns={[{label:'Name'},{label:'Command'},{label:'Args'},{label:'Status'},{label:'Actions'}]}
      rows={mcpServers}
      emptyText="No MCP servers configured"
      loading={mcpLoading}
    >
      {#snippet row(item)}
        {@const srv = item as McpServerInfo}
        <td class="mono">{srv.name}</td>
        <td class="mono">{srv.command}</td>
        <td class="mono dim">{srv.args.join(' ') || '—'}</td>
        <td><Badge color={srv.enabled ? 'green' : 'gray'}>{srv.enabled ? 'enabled' : 'disabled'}</Badge></td>
        <td>
          <div class="row-actions">
            <Button size="sm" variant="danger" onclick={() => doRemoveMcp(srv.name)}>Remove</Button>
          </div>
        </td>
      {/snippet}
    </DataTable>
  {/if}
</div>

<style>
  .page { padding: var(--space-6) var(--space-8); max-width: 900px; }
  h1 { font-size: var(--text-xl); font-weight: var(--weight-bold); margin-bottom: var(--space-5); }

  .tabs { display: flex; gap: 2px; border-bottom: 1px solid var(--color-border); margin-bottom: var(--space-6); }
  .tab {
    background: none; border: none; border-bottom: 2px solid transparent;
    cursor: pointer; font-size: var(--text-sm); color: var(--color-text-dim);
    padding: var(--space-2) var(--space-4); margin-bottom: -1px;
    transition: color var(--transition-fast), border-color var(--transition-fast);
  }
  .tab:hover { color: var(--color-text); }
  .tab.active { color: var(--color-accent); border-bottom-color: var(--color-accent); }

  .section { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-md); padding: var(--space-4); margin-bottom: var(--space-4); }
  .section-title { font-size: var(--text-sm); font-weight: var(--weight-semibold); color: var(--color-text-dim); margin-bottom: var(--space-3); text-transform: uppercase; letter-spacing: 0.04em; }

  .form-row { display: flex; gap: var(--space-2); align-items: center; }
  .form-grid { display: grid; grid-template-columns: 1fr 1fr; gap: var(--space-2); margin-bottom: var(--space-3); }
  .input {
    background: var(--color-bg); border: 1px solid var(--color-border); border-radius: var(--radius-md);
    color: var(--color-text); font-size: var(--text-sm); padding: var(--space-2) var(--space-3); outline: none;
    font-family: var(--font-sans); width: 100%;
  }
  .input:focus { border-color: var(--color-accent); }
  .textarea { resize: vertical; min-height: 64px; font-family: var(--font-mono); font-size: var(--text-xs); }
  .flex-1 { flex: 1; }

  .mono { font-family: var(--font-mono); font-size: var(--text-xs); }
  .dim  { color: var(--color-text-dim); }
  .row-actions { display: flex; gap: var(--space-1); }
</style>
