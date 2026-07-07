<script lang="ts">
  import IconButton from '$lib/components/primitives/IconButton.svelte';
  import SidebarToggleIcon from '$lib/components/primitives/icons/SidebarToggleIcon.svelte';
  import AppBrand from '$lib/components/AppBrand.svelte';
  import SearchButton from '$lib/components/SearchButton.svelte';
  import ThemeMenu from '$lib/components/ThemeMenu.svelte';
  import UserMenu from '$lib/components/UserMenu.svelte';
  import { getSidebarOpen, toggleSidebar } from '$lib/stores/sidebar.svelte';
  import { sessionSnapshot } from '$lib/stores/session.svelte';

  interface Props {
    onSearch: () => void;
  }
  let { onSearch }: Props = $props();

  const sidebarOpen = $derived(getSidebarOpen());
  const session = $derived(sessionSnapshot());
</script>

<header class="topbar">
  <div class="side">
    {#if session.kind === 'signed-in'}
      <IconButton
        onclick={toggleSidebar}
        title="Toggle sidebar (⌘\)"
        ariaLabel={sidebarOpen ? 'Close sidebar' : 'Open sidebar'}
      >
        <SidebarToggleIcon />
      </IconButton>
    {/if}
    <AppBrand />
  </div>

  <div class="side">
    {#if session.kind === 'signed-in'}
      <SearchButton onclick={onSearch} />
    {/if}
    <ThemeMenu />
    {#if session.kind === 'signed-in'}
      <UserMenu user={session.user} />
    {/if}
  </div>
</header>

<style>
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
  .side {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
</style>
