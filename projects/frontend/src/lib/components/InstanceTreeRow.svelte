<script lang="ts">
  import StatusDot from './StatusDot.svelte';
  import IconButton from './IconButton.svelte';
  import { cpuPct, memPct } from '$lib/utils/sysMetrics';
  import type { Instance } from '$lib/types/instance';

  interface Props {
    inst: Instance;
    prefix: string;
    hasChildren: boolean;
    collapsed: boolean;
    onactivate: () => void;
    ontoggle: () => void;
  }
  let { inst, prefix, hasChildren, collapsed, onactivate, ontoggle }: Props = $props();
</script>

<div
  class="tree-row"
  class:down={inst.health === 'down'}
  onclick={onactivate}
  role="button"
  tabindex="0"
  onkeydown={(e) => e.key === 'Enter' && onactivate()}
>
  <span class="tree-prefix" aria-hidden="true">{prefix}</span>
  {#if hasChildren}
    <IconButton
      title={collapsed ? 'Expand' : 'Collapse'}
      onclick={(e) => { e.stopPropagation(); ontoggle(); }}
    >{collapsed ? '▸' : '▾'}</IconButton>
  {:else}
    <span class="tree-toggle-spacer" aria-hidden="true"></span>
  {/if}
  <StatusDot ok={inst.health === 'up' ? true : inst.health === 'down' ? false : null} />
  <span class="hostname">{inst.sys?.hostname ?? inst.label}</span>
  {#if inst.sys?.system_type}<span class="badge-sm">{inst.sys.system_type}</span>{/if}
  {#if inst.version}<span class="meta-sm">v{inst.version}</span>{/if}
  {#if inst.updateAvailable}
    <span class="update-badge" title="Update available: {inst.updateLatest ?? 'newer version'}">
      ↑ {inst.updateLatest ?? 'update'}
    </span>
  {/if}
  <span class="tree-stats">
    CPU {cpuPct(inst.sys) != null ? `${cpuPct(inst.sys)!.toFixed(0)}%` : '—'} ·
    RAM {memPct(inst.sys).toFixed(0)}%
  </span>
</div>

<style>
  .tree-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--text-sm);
    color: var(--text);
  }
  .tree-row:hover { background: var(--surface); }
  .tree-row.down { opacity: 0.6; }
  .tree-prefix {
    font-family: var(--font-mono);
    color: var(--color-text-dim);
    white-space: pre;
    font-size: var(--text-sm);
    line-height: 1;
  }
  .tree-toggle-spacer {
    display: inline-block;
    width: calc(var(--space-1) * 2 + 0.6em);
  }
  .hostname { font-weight: var(--weight-medium); }
  .tree-stats {
    margin-left: auto;
    color: var(--muted);
    font-size: var(--text-xs);
    font-variant-numeric: tabular-nums;
  }
  .badge-sm {
    background: var(--code-bg);
    color: var(--muted);
    padding: 1px var(--space-2);
    border-radius: 999px;
    font-size: var(--text-xs);
  }
  .meta-sm { font-size: var(--text-xs); color: var(--muted); }
  .update-badge {
    font-size: var(--text-xs);
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 999px;
    background: color-mix(in srgb, #f59e0b 25%, transparent);
    color: #f5a623;
    border: 1px solid color-mix(in srgb, #f59e0b 60%, transparent);
    white-space: nowrap;
    flex-shrink: 0;
  }
</style>
