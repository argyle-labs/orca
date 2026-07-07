<script lang="ts">
  import type { Snippet } from 'svelte';
  interface Props {
    color?: 'green' | 'yellow' | 'red' | 'blue' | 'gray' | 'purple' | 'accent';
    children: Snippet;
  }
  let { color = 'gray', children }: Props = $props();
  const colors: Record<string, string> = {
    green: 'var(--color-success)',
    yellow: 'var(--color-warning)',
    red: 'var(--color-error)',
    blue: 'var(--color-info)',
    purple: '#a855f7',
    accent: 'var(--color-accent)',
    gray: 'var(--color-text-dim)',
  };
</script>

<span class="badge" style="--badge-color: {colors[color] ?? colors.gray}">
  {@render children()}
</span>

<style>
  .badge {
    display: inline-flex;
    align-items: center;
    padding: 1px var(--space-2);
    border-radius: 100px;
    font-size: var(--text-xs);
    font-weight: 500;
    border: 1px solid color-mix(in srgb, var(--badge-color) 40%, transparent);
    background: color-mix(in srgb, var(--badge-color) 15%, transparent);
    color: var(--badge-color);
  }
</style>
