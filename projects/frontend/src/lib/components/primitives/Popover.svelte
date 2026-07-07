<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    open = $bindable(false),
    align = 'start',
    width,
    trigger,
    children,
  }: {
    open?: boolean;
    align?: 'start' | 'end';
    width?: number;
    trigger: Snippet;
    children: Snippet;
  } = $props();

  let anchorEl: HTMLElement | null = $state(null);
  let dropdownEl: HTMLElement | null = $state(null);

  function handleOutside(e: MouseEvent) {
    if (!open) return;
    if (anchorEl?.contains(e.target as Node)) return;
    if (dropdownEl?.contains(e.target as Node)) return;
    open = false;
  }
</script>

<svelte:document onclick={handleOutside} />

<div class="popover-root" bind:this={anchorEl}>
  {@render trigger()}
  {#if open}
    <div
      class="popover-dropdown popover-{align}"
      style={width ? `width:${width}px` : ''}
      bind:this={dropdownEl}
    >
      {@render children()}
    </div>
  {/if}
</div>

<style>
  .popover-root { position: relative; display: inline-flex; }
  .popover-dropdown {
    position: absolute;
    top: calc(100% + 4px);
    z-index: var(--z-popover);
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-lg);
    min-width: 160px;
  }
  .popover-start { left: 0; }
  .popover-end   { right: 0; }
</style>
