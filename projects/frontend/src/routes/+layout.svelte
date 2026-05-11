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
  import {
    getSidebarOpen,
    toggleSidebar,
    initSidebarMediaListener,
  } from '$lib/stores/sidebar.svelte';
  import {
    isCommandPaletteOpen,
    toggleCommandPalette,
  } from '$lib/stores/commandPalette.svelte';
  import Notification from '$lib/components/Notification.svelte';
  import ThemeMenu from '$lib/components/ThemeMenu.svelte';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import CommandPalette from '$lib/components/CommandPalette.svelte';

  let { children } = $props();

  const sidebarOpen = $derived(getSidebarOpen());
  const paletteOpen = $derived(isCommandPaletteOpen());

  $effect(() => {
    document.documentElement.setAttribute('data-theme', getPalette());
    document.documentElement.setAttribute('data-mode', getMode());
    const px = FONT_SIZES.find((f) => f.id === getFontSize())?.px ?? 15;
    document.documentElement.style.fontSize = `${px}px`;
  });

  onMount(() => {
    const stopHealth = serverHealth.start();
    const stopBp = initSidebarMediaListener();

    // Re-enable transitions only after the first paint settles. The class is
    // added by the inline script in app.html so first paint is static; we
    // remove it after the browser has had a chance to paint the initial state.
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        document.documentElement.classList.remove('no-transitions');
      });
    });

    return () => {
      stopHealth?.();
      stopBp?.();
    };
  });

  function handleKeydown(e: KeyboardEvent) {
    const meta = e.metaKey || e.ctrlKey;
    if (meta && e.key === 'k') {
      e.preventDefault();
      toggleCommandPalette();
    }
    if (meta && e.key === '\\') {
      e.preventDefault();
      toggleSidebar();
    }
  }
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="app">
  <header class="topbar">
    <div class="topbar-left">
      <button
        class="icon-btn"
        onclick={toggleSidebar}
        aria-label={sidebarOpen ? 'Close sidebar' : 'Open sidebar'}
        title="Toggle sidebar (⌘\)"
      >
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
          <rect x="1" y="1" width="14" height="14" rx="1.5" />
          <line x1="5" y1="1" x2="5" y2="15" />
        </svg>
      </button>
      <a href="/" class="brand">orca</a>
    </div>

    <div class="topbar-right">
      <button class="search-btn" onclick={toggleCommandPalette} title="Search & commands (⌘K)">
        <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8">
          <circle cx="6.5" cy="6.5" r="4.5" />
          <line x1="10.5" y1="10.5" x2="14" y2="14" />
        </svg>
        <span class="search-label">Search</span>
        <kbd>⌘K</kbd>
      </button>

      <ThemeMenu />
    </div>
  </header>

  {#if $serverHealth === 'down'}
    <div class="server-banner">
      Orca backend unreachable —
      <button onclick={() => serverHealth.retry()}>retry</button>
    </div>
  {/if}

  <div class="body">
    <Sidebar />
    <main class="content">{@render children()}</main>
  </div>
</div>

<CommandPalette />
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
    padding: 0 var(--space-3);
    height: var(--nav-height);
    border-bottom: 1px solid var(--color-border);
    background: var(--color-surface);
    flex-shrink: 0;
  }
  .topbar-left, .topbar-right {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }

  .brand {
    font-weight: var(--weight-semibold);
    letter-spacing: 0.04em;
    color: var(--color-text);
    text-decoration: none;
    margin-left: var(--space-2);
  }

  .icon-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 26px;
    background: transparent;
    color: var(--color-text-muted);
    border: 1px solid transparent;
    border-radius: 4px;
    cursor: pointer;
  }
  .icon-btn:hover { background: var(--color-surface-2); color: var(--color-text); }

  .search-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 26px;
    padding: 0 10px;
    background: var(--color-bg);
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    cursor: pointer;
    font-size: var(--text-xs);
  }
  .search-btn:hover { background: var(--color-surface-2); color: var(--color-text); }
  .search-label { min-width: 160px; text-align: left; }
  kbd {
    font-family: var(--font-mono);
    font-size: 10px;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 0 4px;
    color: var(--color-text-dim);
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

  .body {
    flex: 1;
    display: flex;
    min-height: 0;
  }
  .content {
    flex: 1;
    overflow-y: auto;
    background: var(--color-bg);
  }

  /* Narrow viewports: hide the search label so the input shrinks */
  @media (max-width: 768px) {
    .search-label { display: none; min-width: 0; }
  }
</style>
