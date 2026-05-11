<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { serverHealth } from '$lib/stores/serverHealth';
  import {
    getPalette,
    getMode,
    getFontSize,
    FONT_SIZES,
  } from '$lib/stores/theme.svelte';
  import Notification from '$lib/components/Notification.svelte';
  import StatusDot from '$lib/components/StatusDot.svelte';
  import ThemeMenu from '$lib/components/ThemeMenu.svelte';

  let { children } = $props();

  $effect(() => {
    document.documentElement.setAttribute('data-theme', getPalette());
    document.documentElement.setAttribute('data-mode', getMode());
    const px = FONT_SIZES.find((f) => f.id === getFontSize())?.px ?? 15;
    document.documentElement.style.fontSize = `${px}px`;
  });

  onMount(() => serverHealth.start());
</script>

<div class="app">
  <header class="topbar">
    <a href="/" class="brand">orca</a>

    <div class="topbar-right">
      <span class="health">
        <StatusDot ok={$serverHealth === 'up' ? true : $serverHealth === 'down' ? false : null} />
        {$serverHealth}
      </span>

      <ThemeMenu />
    </div>
  </header>

  {#if $serverHealth === 'down'}
    <div class="server-banner">
      Orca backend unreachable —
      <button onclick={() => serverHealth.retry()}>retry</button>
    </div>
  {/if}

  <main class="content">{@render children()}</main>
</div>

<Notification />

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
    background: var(--color-bg);
    color: var(--color-text);
  }
  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 var(--space-4);
    height: var(--nav-height);
    border-bottom: 1px solid var(--color-border);
    background: var(--color-surface);
  }
  .brand {
    font-weight: var(--weight-semibold);
    letter-spacing: 0.04em;
    color: var(--color-text);
    text-decoration: none;
  }
  .topbar-right {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    font-size: var(--text-xs);
  }
  .health {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--color-text-muted);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: 11px;
  }
  .content {
    flex: 1;
    overflow-y: auto;
  }
  .server-banner {
    background: rgba(248, 113, 113, 0.12);
    border-bottom: 1px solid rgba(248, 113, 113, 0.3);
    color: var(--color-error);
    font-size: var(--text-xs);
    padding: 5px var(--space-4);
    text-align: center;
  }
  .server-banner button {
    background: none;
    border: none;
    cursor: pointer;
    color: var(--color-error);
    font-size: var(--text-xs);
    text-decoration: underline;
    padding: 0;
  }
</style>
