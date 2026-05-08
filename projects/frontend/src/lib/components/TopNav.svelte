<script lang="ts">
  import { getSidebarOpen, toggleSidebar, getSidebarSections } from '$lib/stores/sidebar.svelte';
  import { getSection, setSection } from '$lib/stores/mode.svelte';
  import { getPalette, setPalette, toggleMode, getMode, PALETTES, getFontSize, setFontSize, FONT_SIZES, type Palette } from '$lib/stores/theme.svelte';

  let { onsearchopen, oncommandopen }: {
    onsearchopen?: () => void;
    oncommandopen?: () => void;
  } = $props();

  let sidebarOpen  = $derived(getSidebarOpen());
  let sections     = $derived(getSidebarSections());
  let section      = $derived(getSection());
  let palette      = $derived(getPalette());
  let mode         = $derived(getMode());
  let fontSize     = $derived(getFontSize());
  let menuOpen     = $state(false);

  const currentPalette = $derived(PALETTES.find(p => p.id === palette)!);
</script>

<svelte:window onclick={(e) => {
  if (!(e.target as HTMLElement).closest('.nav-menu-wrap')) menuOpen = false;
}} />

<header class="topnav">
  <div class="topnav-left">
    <button class="topnav-icon-btn" onclick={toggleSidebar} title={sidebarOpen ? 'Close sidebar' : 'Open sidebar'}>
      <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
        <rect x="1" y="1" width="14" height="14" rx="1.5"/>
        <line x1="5" y1="1" x2="5" y2="15"/>
      </svg>
    </button>

    {#if sidebarOpen && sections.length > 1}
      <div class="mode-tabs">
        {#each sections as s (s)}
          <button class="mode-tab {section === s ? 'active' : ''}" onclick={() => setSection(s)}>
            {s.charAt(0).toUpperCase() + s.slice(1)}
          </button>
        {/each}
      </div>
    {/if}
  </div>

  <div class="topnav-right">
    {#if onsearchopen}
      <button class="topnav-icon-btn topnav-search-btn" onclick={onsearchopen} title="Search (⌘K)">
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8">
          <circle cx="6.5" cy="6.5" r="4.5"/><line x1="10.5" y1="10.5" x2="14" y2="14"/>
        </svg>
        <span class="search-hint">⌘K</span>
      </button>
    {/if}

    <div class="nav-menu-wrap">
      <button class="topnav-icon-btn" onclick={() => menuOpen = !menuOpen} title="Menu">
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
          <circle cx="8" cy="3" r="1" fill="currentColor" stroke="none"/>
          <circle cx="8" cy="8" r="1" fill="currentColor" stroke="none"/>
          <circle cx="8" cy="13" r="1" fill="currentColor" stroke="none"/>
        </svg>
      </button>

      {#if menuOpen}
        <div class="nav-menu">
          <!-- mode toggle -->
          <div class="menu-section">
            <div class="menu-section-label">Mode</div>
            <div class="menu-mode-row">
              <button
                class="menu-mode-btn {mode === 'dark' ? 'active' : ''}"
                onclick={() => { if (mode !== 'dark') toggleMode(); }}
              >
                <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
                  <path d="M13.5 10.5A6 6 0 0 1 5.5 2.5a6 6 0 1 0 8 8z"/>
                </svg>
                Dark
              </button>
              <button
                class="menu-mode-btn {mode === 'light' ? 'active' : ''}"
                onclick={() => { if (mode !== 'light') toggleMode(); }}
              >
                <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
                  <circle cx="8" cy="8" r="3.5"/>
                  <line x1="8" y1="1" x2="8" y2="2.5"/>
                  <line x1="8" y1="13.5" x2="8" y2="15"/>
                  <line x1="1" y1="8" x2="2.5" y2="8"/>
                  <line x1="13.5" y1="8" x2="15" y2="8"/>
                  <line x1="3.1" y1="3.1" x2="4.2" y2="4.2"/>
                  <line x1="11.8" y1="11.8" x2="12.9" y2="12.9"/>
                  <line x1="12.9" y1="3.1" x2="11.8" y2="4.2"/>
                  <line x1="4.2" y1="11.8" x2="3.1" y2="12.9"/>
                </svg>
                Light
              </button>
            </div>
          </div>

          <div class="menu-divider"></div>

          <!-- palette -->
          <div class="menu-section">
            <div class="menu-section-label">Theme</div>
            {#each PALETTES as p (p.id)}
              <button
                class="menu-item {palette === p.id ? 'active' : ''}"
                onclick={() => { setPalette(p.id); menuOpen = false; }}
              >
                <span class="menu-item-symbol">{p.symbol}</span>
                <span class="menu-item-label">{p.label}</span>
                {#if palette === p.id}<span class="menu-item-check">✓</span>{/if}
              </button>
            {/each}
          </div>

          <div class="menu-divider"></div>

          <!-- font size -->
          <div class="menu-section">
            <div class="menu-section-label">Size</div>
            <div class="menu-font-row">
              {#each FONT_SIZES as f (f.id)}
                <button
                  class="menu-font-btn {fontSize === f.id ? 'active' : ''}"
                  onclick={() => setFontSize(f.id)}
                  title={f.label}
                  style="font-size:{f.px - 2}px"
                >{f.symbol}</button>
              {/each}
            </div>
          </div>

          <div class="menu-divider"></div>

          <!-- nav links -->
          <a href="/plugins" class="menu-item" onclick={() => menuOpen = false}>
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
              <rect x="1.5" y="1.5" width="5" height="5" rx="0.75"/>
              <rect x="9.5" y="1.5" width="5" height="5" rx="0.75"/>
              <rect x="1.5" y="9.5" width="5" height="5" rx="0.75"/>
              <rect x="9.5" y="9.5" width="5" height="5" rx="0.75"/>
            </svg>
            <span class="menu-item-label">Plugins</span>
          </a>
          <a href="/settings" class="menu-item" onclick={() => menuOpen = false}>
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
              <line x1="2" y1="4" x2="14" y2="4"/>
              <line x1="2" y1="8" x2="14" y2="8"/>
              <line x1="2" y1="12" x2="14" y2="12"/>
              <circle cx="5" cy="4" r="1.5" fill="currentColor" stroke="none"/>
              <circle cx="10" cy="8" r="1.5" fill="currentColor" stroke="none"/>
              <circle cx="6" cy="12" r="1.5" fill="currentColor" stroke="none"/>
            </svg>
            <span class="menu-item-label">Settings</span>
          </a>
          <a href="/system" class="menu-item" onclick={() => menuOpen = false}>
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
              <rect x="1" y="3" width="14" height="9" rx="1.5"/>
              <line x1="5" y1="13" x2="11" y2="13"/>
              <line x1="8" y1="12" x2="8" y2="13"/>
              <line x1="4" y1="6" x2="4" y2="9"/>
              <line x1="7" y1="5" x2="7" y2="9"/>
              <line x1="10" y1="7" x2="10" y2="9"/>
            </svg>
            <span class="menu-item-label">System</span>
          </a>
        </div>
      {/if}
    </div>
  </div>
</header>

<style>
  .topnav {
    position: sticky;
    top: 0;
    height: var(--nav-height);
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 var(--space-2) 0 var(--space-1);
    background: var(--color-surface);
    border-bottom: 1px solid var(--color-border);
    z-index: 50;
  }

  .topnav-left  { display: flex; align-items: center; gap: var(--space-1); }
  .topnav-right { display: flex; align-items: center; gap: var(--space-1); }

  .mode-tabs { display: flex; gap: 2px; }
  .mode-tab {
    padding: 3px 8px;
    font-size: 0.6875rem;
    font-weight: var(--weight-medium);
    color: var(--color-text-dim);
    background: none;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    cursor: pointer;
    white-space: nowrap;
    transition: color var(--transition-fast), background var(--transition-fast), border-color var(--transition-fast);
    letter-spacing: 0.01em;
  }
  .mode-tab:hover  { color: var(--color-text-muted); background: var(--color-surface-2); }
  .mode-tab.active { color: var(--color-accent); background: rgba(124,106,247,0.12); border-color: rgba(124,106,247,0.25); }

  .topnav-icon-btn {
    width: 32px; height: 32px; display: flex; align-items: center; justify-content: center;
    border-radius: var(--radius-sm); color: var(--color-text-dim); font-size: 14px;
    text-decoration: none; background: none; border: none; cursor: pointer;
    transition: background var(--transition-fast), color var(--transition-fast);
  }
  .topnav-icon-btn:hover { background: var(--color-surface-2); color: var(--color-text); }

  .topnav-search-btn { width: auto; gap: var(--space-1); padding: 0 var(--space-2); }
  .search-hint { font-size: var(--text-xs); color: var(--color-text-faint); font-family: var(--font-mono); }

  /* nav menu dropdown */
  .nav-menu-wrap { position: relative; }

  .nav-menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    min-width: 160px;
    z-index: var(--z-popover);
    padding: var(--space-1) 0;
  }

  .menu-section { padding: var(--space-1) 0; }
  .menu-section-label {
    padding: var(--space-1) var(--space-3);
    font-size: var(--text-xs);
    font-weight: var(--weight-semibold);
    color: var(--color-text-faint);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .menu-mode-row { display: flex; gap: 4px; padding: 2px var(--space-2) var(--space-1); }
  .menu-mode-btn {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 5px;
    padding: 5px 8px;
    border-radius: var(--radius-sm);
    background: none;
    border: 1px solid var(--color-border);
    color: var(--color-text-dim);
    font-size: var(--text-xs);
    cursor: pointer;
    transition: background var(--transition-fast), color var(--transition-fast), border-color var(--transition-fast);
  }
  .menu-mode-btn:hover  { background: var(--color-surface-2); color: var(--color-text); }
  .menu-mode-btn.active { background: var(--color-surface-2); color: var(--color-accent); border-color: var(--color-accent); }

  .menu-divider { height: 1px; background: var(--color-border); margin: var(--space-1) 0; }

  .menu-item {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    padding: var(--space-2) var(--space-3);
    background: none;
    border: none;
    cursor: pointer;
    font-size: var(--text-sm);
    color: var(--color-text-dim);
    text-decoration: none;
    transition: background var(--transition-fast), color var(--transition-fast);
  }
  .menu-item:hover  { background: var(--color-surface-2); color: var(--color-text); text-decoration: none; }
  .menu-item.active { color: var(--color-accent); }
  .menu-item-symbol { font-size: 14px; width: 18px; text-align: center; flex-shrink: 0; }
  .menu-item-label  { flex: 1; text-align: left; }
  .menu-item-check  { font-size: var(--text-xs); color: var(--color-accent); }

  .menu-font-row { display: flex; gap: 4px; padding: 2px var(--space-2) var(--space-1); }
  .menu-font-btn {
    flex: 1;
    padding: 5px 4px;
    border-radius: var(--radius-sm);
    background: none;
    border: 1px solid var(--color-border);
    color: var(--color-text-dim);
    font-weight: var(--weight-semibold);
    font-family: var(--font-sans);
    cursor: pointer;
    transition: background var(--transition-fast), color var(--transition-fast), border-color var(--transition-fast);
    line-height: 1.2;
  }
  .menu-font-btn:hover  { background: var(--color-surface-2); color: var(--color-text); }
  .menu-font-btn.active { background: var(--color-surface-2); color: var(--color-accent); border-color: var(--color-accent); }
</style>
