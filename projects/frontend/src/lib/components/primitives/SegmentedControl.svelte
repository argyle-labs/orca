<script lang="ts" generics="T extends string | number">
  import type { Snippet } from 'svelte';

  interface Item {
    label: string;
    value: T;
    title?: string;
  }
  interface Props {
    items: Item[];
    value: T;
    onchange: (v: T) => void;
    disabled?: boolean;
    ariaLabel?: string;
    /** Trailing snippet rendered after the last item — for popover-driven
        "custom" segments that need their own trigger element. */
    trailing?: Snippet;
  }
  let { items, value, onchange, disabled = false, ariaLabel, trailing }: Props = $props();
</script>

<div class="segment" role="radiogroup" aria-label={ariaLabel}>
  {#each items as it}
    <button
      type="button"
      class="seg-btn"
      class:is-active={value === it.value}
      role="radio"
      aria-checked={value === it.value}
      disabled={disabled}
      title={it.title}
      onclick={() => onchange(it.value)}
    >{it.label}</button>
  {/each}
  {#if trailing}{@render trailing()}{/if}
</div>

<style>
  .segment {
    display: inline-flex;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    overflow: hidden;
  }
  .seg-btn {
    background: transparent;
    border: none;
    border-left: 1px solid var(--color-border);
    padding: var(--space-1) var(--space-3);
    font: inherit;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    cursor: pointer;
    white-space: nowrap;
  }
  .seg-btn:first-child { border-left: none; }
  .seg-btn:hover:not(:disabled):not(.is-active) {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .seg-btn.is-active {
    background: var(--color-accent);
    color: white;
  }
  .seg-btn:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
