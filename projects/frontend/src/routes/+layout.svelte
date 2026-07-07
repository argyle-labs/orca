<script lang="ts">
  import '../app.css';
  import '$lib/clientConfig';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { serverHealth } from '$lib/stores/serverHealth';
  import { toggleSidebar, initSidebarMediaListener } from '$lib/stores/sidebar.svelte';
  import { sessionSnapshot, refreshSession } from '$lib/stores/session.svelte';
  import { applyAuthRedirect } from '$lib/stores/authRedirect.svelte';
  import { applyThemeToDocument } from '$lib/stores/themeApply.svelte';
  import { paletteLoader } from '$lib/stores/paletteLoader.svelte';
  import Notification from '$lib/components/Notification.svelte';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import AppTopbar from '$lib/components/AppTopbar.svelte';
  import ServerHealthBanner from '$lib/components/ServerHealthBanner.svelte';
  import CommandPaletteHost from '$lib/components/CommandPaletteHost.svelte';

  let { children } = $props();

  const session = $derived(sessionSnapshot());

  $effect(() => {
    applyAuthRedirect(page.url.pathname);
  });

  $effect(() => {
    applyThemeToDocument();
  });

  onMount(() => {
    const stopHealth = serverHealth.start();
    const stopBp = initSidebarMediaListener();
    refreshSession();

    // Re-enable transitions only after first paint settles. The class is
    // added by the inline script in app.html so first paint is static.
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
      void paletteLoader.open();
    }
    if (meta && e.key === '\\') {
      e.preventDefault();
      toggleSidebar();
    }
  }
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="app">
  <AppTopbar onSearch={() => paletteLoader.open()} />
  <ServerHealthBanner />
  <div class="body">
    {#if session.kind === 'signed-in'}
      <Sidebar />
    {/if}
    <main class="content">{@render children()}</main>
  </div>
</div>

<CommandPaletteHost />
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
</style>
