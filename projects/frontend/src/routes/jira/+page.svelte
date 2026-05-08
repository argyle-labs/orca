<script lang="ts">
  import Badge from '$lib/components/Badge.svelte';
  import WorkspaceSetup from '$lib/components/WorkspaceSetup.svelte';
  import { setPluginData, listJiraIssues } from '$lib/api/client';
  import { invalidateAll } from '$app/navigation';

  let { data } = $props();
  let issues: any[] = $derived(data.issues);
  let config: any = $derived(data.config);

  const statusColor = (s: string) =>
    s === 'Done' ? 'green' : s === 'In Progress' ? 'blue' : s === 'Blocked' ? 'red' : 'gray';

  async function saveConfig(values: Record<string, string>) {
    const jql = values.jira_project
      ? `project = ${values.jira_project} ORDER BY updated DESC`
      : 'assignee = currentUser() ORDER BY updated DESC';
    await setPluginData({ id: 'rebuy', key: 'jira_config', body: { value: JSON.stringify({ jql, project: values.jira_project }) } });
    await invalidateAll();
  }
</script>

<svelte:head><title>Jira — orca</title></svelte:head>

<div class="page">
  {#if !config}
    <WorkspaceSetup
      service="Jira"
      fields={[
        { key: 'jira_project', label: 'Project Key', placeholder: 'PROJ', hint: 'Leave blank to show issues assigned to you', required: false },
      ]}
      onSave={saveConfig}
    />
  {:else}
    <div class="header">
      <h1>Jira</h1>
      <button class="reconfigure" onclick={() => setPluginData({ id: 'rebuy', key: 'jira_config', body: { value: '' } }).then(() => invalidateAll())}>
        Reconfigure
      </button>
    </div>
    {#if issues.length === 0}
      <p style="color: var(--color-text-dim)">No issues found.</p>
    {:else}
      <table class="data-table">
        <thead><tr><th>Key</th><th>Summary</th><th>Status</th><th>Assignee</th></tr></thead>
        <tbody>
          {#each issues as issue}
            <tr>
              <td><a href="https://rebuyengine.atlassian.net/browse/{issue.key}" target="_blank" style="font-family:var(--font-mono)">{issue.key}</a></td>
              <td>{issue.fields?.summary ?? '—'}</td>
              <td><Badge color={statusColor(issue.fields?.status?.name ?? '')}>{issue.fields?.status?.name ?? '—'}</Badge></td>
              <td>{issue.fields?.assignee?.displayName ?? 'Unassigned'}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  {/if}
</div>

<style>
  .header { display: flex; align-items: center; gap: var(--space-4); margin-bottom: var(--space-4); }
  .header h1 { margin: 0; }
  .reconfigure { background: none; border: none; cursor: pointer; font-size: var(--text-xs); color: var(--color-text-dim); padding: 2px 6px; border-radius: var(--radius-sm); }
  .reconfigure:hover { color: var(--color-text); background: var(--color-surface-2); }
  .data-table { width: 100%; border-collapse: collapse; font-size: var(--text-sm); }
  .data-table th, .data-table td { padding: var(--space-2) var(--space-3); border-bottom: 1px solid var(--color-border); text-align: left; }
  .data-table th { color: var(--color-text-dim); font-weight: 500; }
</style>
