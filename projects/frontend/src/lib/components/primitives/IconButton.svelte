<script lang="ts">
  import type { Snippet } from 'svelte';
  interface Props {
    title?: string;
    ariaLabel?: string;
    disabled?: boolean;
    onclick?: (e: MouseEvent) => void;
    children: Snippet;
  }
  let { title, ariaLabel, disabled = false, onclick, children }: Props = $props();
</script>

<button
  type="button"
  class="icon-btn"
  {title}
  aria-label={ariaLabel ?? title}
  {disabled}
  {onclick}
>
  {@render children()}
</button>

<style>
  .icon-btn {
    background: transparent;
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    width: 24px;
    height: 22px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    flex-shrink: 0;
    font-size: var(--text-sm);
    line-height: 1;
    padding: 0;
  }
  .icon-btn:hover:not(:disabled) {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .icon-btn:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
