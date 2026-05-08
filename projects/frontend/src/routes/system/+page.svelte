<script lang="ts">
  import Button from '$lib/components/Button.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import DataTable from '$lib/components/DataTable.svelte';
  import Spinner from '$lib/components/Spinner.svelte';
  import { act } from '$lib/stores/notifications';
  import { runDockerAction, getDockerServices, getLogs } from '$lib/api/client';
  import type { DockerRuntimeInfo } from '$lib/api/types';
  import { onMount } from 'svelte';

  let { data } = $props();
  let runtimes: DockerRuntimeInfo[] = $derived(data.runtimes ?? []);

  let selectedRuntime = $state('');
  let services: any[] = $state([]);
  let loadingServices = $state(false);
  let logs: string[] = $state([]);
  let logsService = $state('');
  let loadingLogs = $state(false);

  async function loadServices(path: string) {
    selectedRuntime = path;
    loadingServices = true;
    const res = await act(() => getDockerServices({ path }));
    services = (res as any)?.services ?? [];
    loadingServices = false;
  }

  async function doAction(action: string, service: string) {
    if (await act(
      () => runDockerAction({ body: { action, path: selectedRuntime, service } as any }),
      { success: `${action} ${service}` },
    )) await loadServices(selectedRuntime);
  }

  async function viewLogs(service: string) {
    logsService = service;
    loadingLogs = true;
    const res = await act(() => getLogs({ project: selectedRuntime, service, tail: 200 }));
    logs = (res as any)?.lines ?? [];
    loadingLogs = false;
  }

  const stateColor = (s: string) =>
    s === 'running' ? 'green' : s === 'exited' ? 'red' : s === 'paused' ? 'yellow' : 'gray';

  onMount(() => {
    selectedRuntime = runtimes[0]?.name ?? '';
    if (selectedRuntime) loadServices(selectedRuntime);
  });
</script>

<svelte:head><title>System — orca</title></svelte:head>

<div class="page">
  <h1>System</h1>

  {#if runtimes.length > 1}
    <div class="runtime-tabs">
      {#each runtimes as r}
        <button class="tab" class:active={selectedRuntime === r.name} onclick={() => loadServices(r.name)}>
          {r.name}
        </button>
      {/each}
    </div>
  {/if}

  <DataTable
    columns={[{label:'Service'},{label:'State'},{label:'Image'},{label:'Actions'}]}
    rows={services}
    emptyText="No services"
    loading={loadingServices}
  >
    {#snippet row(item)}
      {@const svc = item as any}
      <td style="font-family:var(--font-mono)">{svc.name}</td>
      <td><Badge color={stateColor(svc.state)}>{svc.state}</Badge></td>
      <td style="font-size:var(--text-xs);color:var(--color-text-dim)">{svc.image ?? '—'}</td>
      <td>
        <div class="actions">
          {#if svc.state !== 'running'}
            <Button size="sm" onclick={() => doAction('start', svc.name)}>Start</Button>
          {:else}
            <Button size="sm" onclick={() => doAction('stop', svc.name)}>Stop</Button>
            <Button size="sm" onclick={() => doAction('restart', svc.name)}>Restart</Button>
          {/if}
          <Button size="sm" variant="ghost" onclick={() => viewLogs(svc.name)}>Logs</Button>
        </div>
      </td>
    {/snippet}
  </DataTable>

  {#if logsService}
    <div class="log-panel">
      <div class="log-header">
        <span style="font-family:var(--font-mono)">{logsService}</span>
        <button class="close-btn" onclick={() => { logsService = ''; logs = []; }}>✕</button>
      </div>
      {#if loadingLogs}
        <div class="center"><Spinner size={20} /></div>
      {:else}
        <pre class="log-output">{logs.join('\n')}</pre>
      {/if}
    </div>
  {/if}
</div>

<style>
  .page { padding: var(--space-6) var(--space-8); max-width: 1100px; }
  h1 { font-size: var(--text-xl); font-weight: var(--weight-bold); margin-bottom: var(--space-5); }
  .runtime-tabs { display: flex; gap: var(--space-2); margin-bottom: var(--space-4); }
  .tab { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); color: var(--color-text-dim); cursor: pointer; padding: var(--space-2) var(--space-4); font-size: var(--text-sm); }
  .tab.active { border-color: var(--color-accent); color: var(--color-text); }
  .center { display: flex; justify-content: center; padding: var(--space-8); }
  .actions { display: flex; gap: var(--space-2); }
  .log-panel { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-md); overflow: hidden; margin-top: var(--space-6); }
  .log-header { display: flex; justify-content: space-between; align-items: center; padding: var(--space-3) var(--space-4); border-bottom: 1px solid var(--color-border); font-size: var(--text-sm); }
  .close-btn { background: none; border: none; color: var(--color-text-dim); cursor: pointer; }
  .log-output { margin: 0; padding: var(--space-4); font-size: var(--text-xs); max-height: 400px; overflow-y: auto; white-space: pre-wrap; word-break: break-all; }
</style>
