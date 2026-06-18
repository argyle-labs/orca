<script lang="ts" generics="T">
  import type { Snippet } from 'svelte';

  let {
    columns,
    rows,
    emptyText = 'No items',
    row: rowSnippet,
    loading = false,
  }: {
    columns: { label: string; width?: string }[];
    rows: T[];
    emptyText?: string;
    row: Snippet<[T]>;
    loading?: boolean;
  } = $props();
</script>

<table class="data-table">
  <thead>
    <tr>
      {#each columns as col (col.label)}
        <th style={col.width ? `width:${col.width}` : ''}>{col.label}</th>
      {/each}
    </tr>
  </thead>
  <tbody>
    {#if loading}
      <tr><td colspan={columns.length} class="empty">Loading…</td></tr>
    {:else}
      {#each rows as item}
        <tr>{@render rowSnippet(item)}</tr>
      {:else}
        <tr><td colspan={columns.length} class="empty">{emptyText}</td></tr>
      {/each}
    {/if}
  </tbody>
</table>

<style>
  .data-table { width: 100%; border-collapse: collapse; font-size: var(--text-sm); }
  .data-table th, .data-table td { padding: var(--space-2) var(--space-3); border-bottom: 1px solid var(--color-border); text-align: left; vertical-align: middle; }
  .data-table th { color: var(--color-text-dim); font-weight: var(--weight-medium); font-size: var(--text-xs); text-transform: uppercase; letter-spacing: 0.04em; }
  .data-table tr:last-child td { border-bottom: none; }
  .empty { color: var(--color-text-faint); font-style: italic; text-align: center; padding: var(--space-6) !important; }
</style>
