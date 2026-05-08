<script lang="ts">
  import Badge from '$lib/components/Badge.svelte';
  import Button from '$lib/components/Button.svelte';
  import { notifications } from '$lib/stores/notifications';
  import { invalidateAll } from '$app/navigation';

  let { data } = $props();

  interface Mapping {
    orca_tool: string; mcp_name: string; external_tool: string;
    match_type: string; confidence: number | null; enabled: boolean;
  }

  let mappings: Mapping[] = $derived(data.mappings ?? []);

  const grouped = $derived(
    mappings.reduce<Record<string, Mapping[]>>((acc, m) => {
      (acc[m.mcp_name] ||= []).push(m);
      return acc;
    }, {})
  );

  const typeColor = (t: string) => t === 'explicit' ? 'green' : t === 'llm_matched' ? 'blue' : 'gray';

  async function unmap(orcaTool: string) {
    try {
      await fetch(`/api/mcp/mappings/${encodeURIComponent(orcaTool)}`, { method: 'DELETE' });
      await invalidateAll();
      notifications.success(`Unmapped ${orcaTool}`);
    } catch (e) { notifications.error(String(e)); }
  }
</script>

<svelte:head><title>MCPs — orca</title></svelte:head>

<div class="page">
  <h1>MCP Federations</h1>

  {#if mappings.length === 0}
    <p style="color:var(--color-text-dim)">No mappings. Use <code>orca mcp map</code> to add one.</p>
  {:else}
    {#each Object.entries(grouped) as [mcpName, rows]}
      <section class="mcp-section">
        <h2 class="mcp-name">{mcpName}</h2>
        <table class="data-table">
          <thead>
            <tr><th>Orca tool</th><th>External tool</th><th>Type</th><th>Confidence</th><th></th></tr>
          </thead>
          <tbody>
            {#each rows as row}
              <tr>
                <td style="font-family:var(--font-mono);font-size:var(--text-xs)">{row.orca_tool}</td>
                <td style="font-family:var(--font-mono);font-size:var(--text-xs)">{row.external_tool}</td>
                <td><Badge color={typeColor(row.match_type)}>{row.match_type}</Badge></td>
                <td style="color:var(--color-text-dim)">
                  {row.confidence != null ? `${(row.confidence * 100).toFixed(0)}%` : '—'}
                </td>
                <td>
                  <Button size="sm" variant="danger" onclick={() => unmap(row.orca_tool)}>unmap</Button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </section>
    {/each}
  {/if}
</div>

<style>
  .mcp-section { margin-bottom: var(--space-8); }
  .mcp-name { font-size: var(--text-lg); color: var(--color-text-dim); margin-bottom: var(--space-3); font-weight: 400; font-family: var(--font-mono); }
  .data-table { width: 100%; border-collapse: collapse; font-size: var(--text-sm); }
  .data-table th, .data-table td { padding: var(--space-2) var(--space-3); border-bottom: 1px solid var(--color-border); text-align: left; }
  .data-table th { color: var(--color-text-dim); font-weight: 500; }
</style>
