<script lang="ts">
  import Spinner from '$lib/components/Spinner.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import WorkspaceSetup from '$lib/components/WorkspaceSetup.svelte';
  import { notifications } from '$lib/stores/notifications';
  import { listBitbucketPRs, setPluginData } from '$lib/api/client';
  import { invalidateAll } from '$app/navigation';
  import type { RepoInfo } from '$lib/api/types';

  let { data } = $props();
  let repos: RepoInfo[] = $derived(data.repos);
  let config: any = $derived(data.config);

  let selectedRepo: RepoInfo | null = $state(null);
  let prs: any[] = $state([]);
  let loading = $state(false);

  async function fetchPRs(repo: RepoInfo) {
    selectedRepo = repo;
    loading = true;
    try {
      const res = await listBitbucketPRs({ workspace: repo.workspace, slug: repo.slug });
      prs = (res as any)?.values ?? [];
    } catch (e) { notifications.error(String(e)); }
    finally { loading = false; }
  }

  async function saveConfig(values: Record<string, string>) {
    await setPluginData({ id: 'rebuy', key: 'bitbucket_config', body: { value: { workspace: values.workspace } } });
    await invalidateAll();
  }

  const prStateColor = (s: string) => s === 'MERGED' ? 'green' : s === 'OPEN' ? 'blue' : s === 'DECLINED' ? 'red' : 'gray';
</script>

<svelte:head><title>Bitbucket — orca</title></svelte:head>

<div class="page">
  {#if !config}
    <WorkspaceSetup
      service="Bitbucket"
      fields={[
        { key: 'workspace', label: 'Workspace', placeholder: 'myworkspace', hint: 'Your Bitbucket workspace slug', required: true },
      ]}
      onSave={saveConfig}
    />
  {:else}
    <div class="header">
      <h1>Bitbucket <span class="ws-badge">{config.workspace}</span></h1>
      <button class="reconfigure" onclick={() => setPluginData({ id: 'rebuy', key: 'bitbucket_config', body: { value: '' } }).then(() => invalidateAll())}>
        Reconfigure
      </button>
    </div>
    <div class="layout">
      <aside class="repo-list">
        {#each repos as repo}
          <button class="repo-btn" class:active={selectedRepo?.slug === repo.slug} onclick={() => fetchPRs(repo)}>
            {repo.slug}
          </button>
        {/each}
      </aside>
      <section class="pr-panel">
        {#if loading}<div class="center"><Spinner size={24} /></div>
        {:else if prs.length > 0}
          <table class="data-table">
            <thead><tr><th>#</th><th>Title</th><th>Author</th><th>State</th></tr></thead>
            <tbody>
              {#each prs as pr}
                <tr>
                  <td><a href={pr.links?.html?.href ?? '#'} target="_blank" style="font-family:var(--font-mono)">#{pr.id}</a></td>
                  <td>{pr.title}</td>
                  <td>{pr.author?.display_name ?? '—'}</td>
                  <td><Badge color={prStateColor(pr.state)}>{pr.state}</Badge></td>
                </tr>
              {/each}
            </tbody>
          </table>
        {:else if selectedRepo}
          <p style="color:var(--color-text-dim)">No open PRs for {selectedRepo.slug}.</p>
        {:else}
          <p style="color:var(--color-text-dim)">Select a repository.</p>
        {/if}
      </section>
    </div>
  {/if}
</div>

<style>
  .header { display: flex; align-items: center; gap: var(--space-3); margin-bottom: var(--space-4); }
  .header h1 { margin: 0; }
  .ws-badge { font-size: var(--text-sm); font-weight: normal; color: var(--color-text-dim); background: var(--color-surface-2); padding: 2px 8px; border-radius: var(--radius-sm); font-family: var(--font-mono); }
  .reconfigure { background: none; border: none; cursor: pointer; font-size: var(--text-xs); color: var(--color-text-dim); padding: 2px 6px; border-radius: var(--radius-sm); margin-left: auto; }
  .reconfigure:hover { color: var(--color-text); background: var(--color-surface-2); }
  .layout { display: grid; grid-template-columns: 200px 1fr; gap: var(--space-6); }
  .repo-list { display: flex; flex-direction: column; gap: var(--space-1); }
  .repo-btn {
    background: none; border: 1px solid transparent; border-radius: var(--radius-sm);
    color: var(--color-text-dim); cursor: pointer; font-size: var(--text-sm);
    padding: var(--space-2) var(--space-3); text-align: left;
  }
  .repo-btn:hover { background: var(--color-surface); color: var(--color-text); }
  .repo-btn.active { background: var(--color-surface); border-color: var(--color-accent); color: var(--color-text); }
  .center { display: flex; justify-content: center; padding: var(--space-8); }
  .data-table { width: 100%; border-collapse: collapse; font-size: var(--text-sm); }
  .data-table th, .data-table td { padding: var(--space-2) var(--space-3); border-bottom: 1px solid var(--color-border); text-align: left; }
  .data-table th { color: var(--color-text-dim); font-weight: 500; }
</style>
