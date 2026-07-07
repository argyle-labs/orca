<script module lang="ts">
  // Module-level scroll cache. Survives component remounts AND is sampled
  // by both scroll events and link clicks so the right value is always in
  // hand the moment SvelteKit nav completes. Lives outside component scope
  // intentionally — Svelte's $state binds to the instance lifecycle.
  let cachedScroll = 0;
</script>

<script lang="ts">
  import { page } from '$app/stores';
  import { afterNavigate } from '$app/navigation';
  import { onMount, tick } from 'svelte';
  import {
    getSidebarOpen,
    isMobileViewport,
    closeSidebar,
  } from '$lib/stores/sidebar.svelte';
  import { NAV_SECTIONS } from '$lib/nav';

  const open = $derived(getSidebarOpen());
  const mobile = $derived(isMobileViewport());
  const pathname = $derived($page.url.pathname);

  let asideEl: HTMLElement | null = $state(null);

  function onScroll(e: Event) {
    cachedScroll = (e.currentTarget as HTMLElement).scrollTop;
  }

  // Restore after SvelteKit's own scroll-restoration has run. The double
  // requestAnimationFrame waits past the paint where SvelteKit may have
  // reset scroll positions during route transitions.
  function restore() {
    if (!asideEl) return;
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        if (asideEl && asideEl.scrollTop !== cachedScroll) {
          asideEl.scrollTop = cachedScroll;
        }
      });
    });
  }

  onMount(() => {
    restore();
  });

  afterNavigate(() => {
    restore();
  });

  function handleNav(item: { href: string; enabled?: boolean }, e: MouseEvent) {
    if (item.enabled === false) {
      e.preventDefault();
      return;
    }
    // Capture scroll before SvelteKit takes over the navigation, in case the
    // user clicked without scrolling first (cachedScroll might be stale).
    if (asideEl) cachedScroll = asideEl.scrollTop;
    if (mobile) closeSidebar();
  }
</script>

{#if open && mobile}
  <!-- Scrim closes the drawer on mobile -->
  <button class="scrim" aria-label="Close sidebar" onclick={() => closeSidebar()}></button>
{/if}

<aside
  class="sidebar"
  aria-hidden={!open}
  inert={!open}
  bind:this={asideEl}
  onscroll={onScroll}
>
  <nav>
    {#each NAV_SECTIONS as section (section.label)}
      <div class="section">
        <div class="section-label">{section.label}</div>
        <ul>
          {#each section.items as item (item.href)}
            <li>
              {#if item.enabled !== true}
                <!--
                  Default: items render as <button disabled>, not <a href>, until
                  their page is built (`enabled: true`). This matters for two
                  reasons:
                    1. No accidental nav to a 404 route, which was scrolling the
                       sidebar back to top on every "soon" click.
                    2. An anchor — even with preventDefault — moves focus to
                       the link, and focusing a tabindex element inside a
                       scroll container can trigger scrollIntoView snapping.
                -->
                <button class="nav-item disabled" type="button" disabled>
                  <span class="icon">{item.icon}</span>
                  <span class="label">{item.label}</span>
                  <span class="badge">soon</span>
                </button>
              {:else}
                <a
                  class="nav-item"
                  class:active={pathname === item.href}
                  href={item.href}
                  onclick={(e) => handleNav(item, e)}
                  data-sveltekit-noscroll
                >
                  <span class="icon">{item.icon}</span>
                  <span class="label">{item.label}</span>
                </a>
              {/if}
            </li>
          {/each}
        </ul>
      </div>
    {/each}
  </nav>
</aside>

<style>
  /*
   * Sidebar visibility is CSS-driven so the first paint (pre-hydration)
   * already matches the viewport — no JS flash, no "appears then slides out".
   *
   * Rules:
   *  - Default: viewport breakpoint decides (mobile = hidden, desktop = shown).
   *  - User pin: `[data-sidebar=open|closed]` on <html> overrides the default.
   *  - The Svelte component still tracks logical state for the toggle button,
   *    scrim rendering, and click-to-close-on-mobile — but visibility itself
   *    is pure CSS.
   */
  .sidebar {
    background: var(--color-surface);
    border-right: 1px solid var(--color-border);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    /* Native thin scrollbar themed off the active palette. Firefox uses the
       standardised properties; WebKit/Blink gets the pseudo-element rules. */
    scrollbar-width: thin;
    scrollbar-color: var(--color-border) transparent;
  }
  .sidebar::-webkit-scrollbar { width: 8px; }
  .sidebar::-webkit-scrollbar-track { background: transparent; }
  .sidebar::-webkit-scrollbar-thumb {
    background: var(--color-border);
    border-radius: 4px;
  }
  .sidebar::-webkit-scrollbar-thumb:hover { background: var(--color-text-dim); }
  /* Suppress transitions on the very first paint so SSR→hydration doesn't
     animate. The .no-transitions class on <html> is removed in onMount. */
  :global(html:not(.no-transitions)) .sidebar {
    transition: transform 0.18s ease, width 0.18s ease;
  }

  /* Mobile drawer (default): hidden off-screen */
  .sidebar {
    position: fixed;
    inset: var(--nav-height) auto 0 0;
    z-index: var(--z-popover);
    box-shadow: var(--shadow-lg);
    width: var(--sidebar-width, 220px);
    transform: translateX(-100%);
  }

  /* Desktop default: laid out as a flex column inline with content */
  @media (min-width: 1024px) {
    .sidebar {
      position: static;
      inset: auto;
      z-index: auto;
      box-shadow: none;
      transform: translateX(0);
      flex-shrink: 0;
    }
  }

  /* User explicitly pinned closed: hide on every viewport */
  :global(html[data-sidebar='closed']) .sidebar {
    transform: translateX(-100%);
    width: 0;
    border-right: none;
    overflow: hidden;
  }
  @media (min-width: 1024px) {
    :global(html[data-sidebar='closed']) .sidebar {
      transform: none;
    }
  }

  /* User explicitly pinned open: show on every viewport.
     On mobile it floats as a drawer (default positioning); on desktop it
     inlines normally (the desktop @media block above already did that). */
  :global(html[data-sidebar='open']) .sidebar {
    transform: translateX(0);
    width: var(--sidebar-width, 220px);
    border-right: 1px solid var(--color-border);
    overflow-y: auto;
  }

  .scrim {
    position: fixed;
    inset: var(--nav-height) 0 0 0;
    background: rgba(0, 0, 0, 0.4);
    z-index: calc(var(--z-popover) - 1);
    border: none;
    cursor: pointer;
    padding: 0;
  }

  nav { padding: var(--space-3) 0; }
  .section { margin-bottom: var(--space-4); }
  .section-label {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--color-text-dim);
    padding: 0 var(--space-4);
    margin-bottom: var(--space-1);
  }
  ul { list-style: none; padding: 0; margin: 0; }

  .nav-item {
    width: 100%;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: 6px var(--space-4);
    color: var(--color-text-muted);
    background: transparent;
    border: none;
    border-left: 2px solid transparent;
    text-decoration: none;
    font-size: var(--text-sm);
    font-family: inherit;
    text-align: left;
    cursor: pointer;
  }
  .nav-item:hover { background: var(--color-surface-2); color: var(--color-text); }
  .nav-item.active {
    background: var(--color-surface-2);
    color: var(--color-text);
    border-left-color: var(--color-accent);
  }
  .nav-item.disabled,
  .nav-item:disabled {
    opacity: var(--opacity-disabled);
    cursor: not-allowed;
  }
  .nav-item.disabled:hover,
  .nav-item:disabled:hover { background: transparent; color: var(--color-text-muted); }

  .icon { width: 16px; text-align: center; color: var(--color-text-dim); font-size: 13px; }
  .nav-item.active .icon { color: var(--color-accent); }
  .label { flex: 1; }
  .badge {
    font-size: 9px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--color-text-dim);
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
  }
</style>
