<script lang="ts">
  import StatusDot from './StatusDot.svelte';
  import type { ControlLayerVariant } from '../types/sidebar';

  interface Link { label: string; href?: string; onclick?: () => void }

  interface Props {
    variant?: ControlLayerVariant;
    title: string;
    source?: string;
    statusOk?: boolean | null;
    links?: Link[];
    onrefresh?: () => void;
    children: import('svelte').Snippet;
  }

  let { variant = 'default', title, source, statusOk, links = [], onrefresh, children }: Props = $props();
</script>

<div class="layer layer-{variant}">
  <div class="layer-header">
    {#if statusOk !== undefined && statusOk !== null}
      <StatusDot ok={statusOk} />
    {:else if statusOk === null}
      <StatusDot ok={null} />
    {/if}
    <span class="layer-title">{title}</span>
    {#if source}<span class="layer-source">{source}</span>{/if}
    {#each links as link (link.label)}
      {#if link.href}
        <a class="layer-link" href={link.href} onclick={link.onclick}>{link.label}</a>
      {:else}
        <button class="layer-link-btn" onclick={link.onclick}>{link.label}</button>
      {/if}
    {/each}
    {#if onrefresh}
      <button class="layer-refresh" onclick={onrefresh}>↺</button>
    {/if}
  </div>
  {@render children()}
</div>

<style>
  .layer {
    border-left: 2px solid var(--color-border);
    padding: var(--space-2) var(--space-3);
    margin-bottom: var(--space-2);
    border-radius: 0 var(--radius-sm) var(--radius-sm) 0;
    background: var(--color-surface);
  }
  .layer-mcp       { border-left-color: #34d399; }
  .layer-compose   { border-left-color: var(--color-accent); }
  .layer-container { border-left-color: var(--color-border); background: none; }
  .layer-default   { border-left-color: var(--color-border); }

  .layer-header {
    display: flex; align-items: center; gap: var(--space-2);
    margin-bottom: var(--space-2); font-size: var(--text-xs);
  }
  .layer-title { font-weight: var(--weight-semibold); color: var(--color-text-muted); text-transform: uppercase; letter-spacing: 0.05em; }
  .layer-source { color: var(--color-text-faint); font-family: var(--font-mono); flex: 1; }
  .layer-link {
    color: var(--color-text-dim); font-size: var(--text-xs); text-decoration: none;
    padding: 1px 4px; border-radius: var(--radius-sm);
    transition: color var(--transition-fast);
  }
  .layer-link:hover { color: var(--color-accent); }
  .layer-link-btn {
    background: none; border: none; cursor: pointer;
    color: var(--color-text-dim); font-size: var(--text-xs);
    padding: 1px 4px; border-radius: var(--radius-sm);
    transition: color var(--transition-fast);
  }
  .layer-link-btn:hover { color: var(--color-accent); }
  .layer-refresh {
    background: none; border: none; cursor: pointer;
    color: var(--color-text-dim); font-size: var(--text-sm);
    padding: 1px 4px; border-radius: var(--radius-sm);
    margin-left: auto;
  }
  .layer-refresh:hover { color: var(--color-accent); }
</style>
