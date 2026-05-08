<script lang="ts">
  import '../app.css';
  import TopNav from '$lib/components/TopNav.svelte';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import SearchModal from '$lib/components/SearchModal.svelte';
  import CommandPalette from '$lib/components/CommandPalette.svelte';
  import HealthDashboard from '$lib/components/HealthDashboard.svelte';
  import Notification from '$lib/components/Notification.svelte';
  import { page } from '$app/stores';
  import { recordNav } from '$lib/stores/navHistory';
  import { serverHealth } from '$lib/stores/serverHealth';
  import { getPalette, getMode, getFontSize, FONT_SIZES } from '$lib/stores/theme.svelte';
  import { onMount } from 'svelte';

  let { children } = $props();

  let searchOpen    = $state(false);
  let commandOpen   = $state(false);
  let healthOpen    = $state(false);

  $effect(() => { recordNav($page.url.pathname); });
  $effect(() => {
    document.documentElement.setAttribute('data-theme', getPalette());
    document.documentElement.setAttribute('data-mode', getMode());
    const px = FONT_SIZES.find(f => f.id === getFontSize())?.px ?? 15;
    document.documentElement.style.fontSize = `${px}px`;
  });

  onMount(() => serverHealth.start());

  function handleKeydown(e: KeyboardEvent) {
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLSelectElement) return;
    const meta = e.metaKey || e.ctrlKey;
    if (meta && e.key === 'k') { e.preventDefault(); searchOpen = !searchOpen; }
    if (meta && e.key === '/') { e.preventDefault(); commandOpen = !commandOpen; }
  }

  const isFullscreen = $derived(false);
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="app">
  <TopNav onsearchopen={() => searchOpen = true} oncommandopen={() => commandOpen = true} />

  {#if $serverHealth === 'down'}
    <div class="server-banner">
      Brain server is unreachable — <button onclick={() => serverHealth.retry()}>retry</button>
    </div>
  {/if}

  <!-- Sidebar is always mounted (for plugin/section loading) but renders as a fixed overlay -->
  <Sidebar onhealthopen={() => healthOpen = true} />

  <div class="app-body">
    <main class="main-content">{@render children()}</main>
  </div>
</div>

<SearchModal    open={searchOpen}  onclose={() => searchOpen  = false} />
<CommandPalette open={commandOpen} onclose={() => commandOpen = false} />
<HealthDashboard open={healthOpen} onclose={() => healthOpen  = false} />
<Notification />

<style>
  .app { display: flex; flex-direction: column; height: 100vh; overflow: hidden; }
  .app-body { display: flex; flex: 1; overflow: hidden; }
  .main-content { flex: 1; overflow-y: auto; display: flex; flex-direction: column; }

  .server-banner {
    background: rgba(248,113,113,0.12);
    border-bottom: 1px solid rgba(248,113,113,0.3);
    color: var(--color-error);
    font-size: var(--text-xs);
    padding: 5px var(--space-4);
    text-align: center;
  }
  .server-banner button {
    background: none; border: none; cursor: pointer;
    color: var(--color-error); font-size: var(--text-xs);
    text-decoration: underline; padding: 0;
  }
</style>
