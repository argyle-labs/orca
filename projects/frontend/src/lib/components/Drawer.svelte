<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    open: boolean;
    side?: 'left' | 'right';
    width?: string;
    onclose?: () => void;
    ariaLabel?: string;
    backdrop?: boolean;
    children: Snippet;
  }

  let {
    open,
    side = 'right',
    width = 'min(420px, 90vw)',
    onclose,
    ariaLabel,
    backdrop = true,
    children,
  }: Props = $props();

  function handleBackdropClick() {
    onclose?.();
  }

  function handleKey(e: KeyboardEvent) {
    if (e.key === 'Escape' && open) onclose?.();
  }
</script>

<svelte:window onkeydown={handleKey} />

{#if backdrop}
  <button
    class="backdrop"
    class:open
    aria-label="Close drawer"
    tabindex={open ? 0 : -1}
    onclick={handleBackdropClick}
  ></button>
{/if}

<aside
  class="drawer {side}"
  class:open
  style="--drawer-width: {width}"
  aria-hidden={!open}
  aria-label={ariaLabel}
  inert={!open}
>
  {@render children()}
</aside>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.4);
    z-index: 100;
    border: none;
    padding: 0;
    cursor: pointer;
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.18s ease;
  }
  .backdrop.open {
    opacity: 1;
    pointer-events: auto;
  }

  .drawer {
    position: fixed;
    top: 0;
    bottom: 0;
    width: var(--drawer-width);
    background: var(--color-surface);
    z-index: 101;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    transition: transform 0.18s ease;
  }
  .drawer.right {
    right: 0;
    border-left: 1px solid var(--color-border);
    transform: translateX(100%);
  }
  .drawer.left {
    left: 0;
    border-right: 1px solid var(--color-border);
    transform: translateX(-100%);
  }
  .drawer.open {
    transform: translateX(0);
  }
</style>
