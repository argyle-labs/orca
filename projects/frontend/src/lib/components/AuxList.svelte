<script lang="ts">
  import type { Snippet } from 'svelte';
  interface Props {
    title: string;
    children: Snippet;
  }
  let { title, children }: Props = $props();
</script>

<div class="aux-section">
  <div class="aux-head">{title}</div>
  <div class="aux-list">
    {@render children()}
  </div>
</div>

<style>
  .aux-section {
    margin-top: var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    overflow: hidden;
  }
  .aux-head {
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface);
    color: var(--color-text-dim);
    font-size: var(--text-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-weight: 600;
  }
  .aux-list { display: flex; flex-direction: column; }
</style>
