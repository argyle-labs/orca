<script lang="ts">
  interface Props {
    cluster: string | null;
    summary?: { total: number; online: number; quorate: boolean | null } | null;
  }
  let { cluster, summary = null }: Props = $props();
</script>

<div
  class="cluster-header"
  aria-label={cluster ? `Proxmox cluster ${cluster}` : 'Ungrouped systems'}
>
  {#if cluster}
    <span class="cluster-label">Cluster: {cluster}</span>
    {#if summary}
      <span class="cluster-meta">
        ({summary.online}/{summary.total} nodes{summary.quorate === false ? ' · not quorate' : ''})
      </span>
    {/if}
  {:else}
    <span class="cluster-label">Ungrouped</span>
  {/if}
</div>

<style>
  .cluster-header {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    margin-top: var(--space-3);
    border-bottom: 1px solid var(--color-border, rgba(255,255,255,0.08));
    color: var(--color-text-muted);
    font-size: var(--text-sm, 0.875rem);
    letter-spacing: 0.02em;
    text-transform: uppercase;
  }
  .cluster-header:first-child {
    margin-top: 0;
  }
  .cluster-label {
    font-weight: 600;
    color: var(--color-text);
  }
  .cluster-meta {
    color: var(--color-text-muted);
    font-size: var(--text-xs, 0.75rem);
  }
</style>
