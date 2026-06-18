<script lang="ts">
  import Popover from './primitives/Popover.svelte';
  import ChevronDownIcon from './primitives/icons/ChevronDownIcon.svelte';
  import CheckIcon from './primitives/icons/CheckIcon.svelte';
  import MoonIcon from './primitives/icons/MoonIcon.svelte';
  import SunIcon from './primitives/icons/SunIcon.svelte';
  import {
    PALETTES,
    FONT_SIZES,
    getPalette,
    setPalette,
    getMode,
    setMode,
    getFontSize,
    setFontSize,
    type Palette,
    type Mode,
    type FontSize,
  } from '$lib/stores/theme.svelte';

  let open = $state(false);

  const palette = $derived(getPalette());
  const mode = $derived(getMode());
  const fontSize = $derived(getFontSize());
</script>

<Popover bind:open align="end" width={220}>
  {#snippet trigger()}
    <button
      class="trigger"
      onclick={() => (open = !open)}
      aria-label="Theme settings"
      title="Theme"
    >
      <span class="swatch" style="background: var(--color-accent)"></span>
      <ChevronDownIcon />
    </button>
  {/snippet}

  {#snippet children()}
    <div class="menu">
      <div class="section">
        <div class="label">Palette</div>
        <div class="palette-list">
          {#each PALETTES as p (p.id)}
            <button
              class="palette-item {palette === p.id ? 'active' : ''}"
              data-theme={p.id}
              data-mode={mode}
              onclick={() => setPalette(p.id as Palette)}
              aria-pressed={palette === p.id}
            >
              <span class="swatch-lg"></span>
              <span class="palette-label">{p.label}</span>
              {#if palette === p.id}
                <span class="check"><CheckIcon /></span>
              {/if}
            </button>
          {/each}
        </div>
      </div>

      <div class="divider"></div>

      <div class="section">
        <div class="label">Mode</div>
        <div class="seg">
          {#each ['dark', 'light'] as m (m)}
            <button
              class="seg-btn {mode === m ? 'active' : ''}"
              onclick={() => setMode(m as Mode)}
              aria-pressed={mode === m}
            >
              {#if m === 'dark'}<MoonIcon />{:else}<SunIcon />{/if}
              {m}
            </button>
          {/each}
        </div>
      </div>

      <div class="divider"></div>

      <div class="section">
        <div class="label">Font size</div>
        <div class="seg">
          {#each FONT_SIZES as f (f.id)}
            <button
              class="seg-btn {fontSize === f.id ? 'active' : ''}"
              onclick={() => setFontSize(f.id as FontSize)}
              aria-pressed={fontSize === f.id}
              style="font-size: {f.px}px"
            >
              {f.symbol}
            </button>
          {/each}
        </div>
      </div>
    </div>
  {/snippet}
</Popover>

<style>
  .trigger {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 26px;
    padding: 0 8px;
    background: var(--color-surface);
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    cursor: pointer;
  }
  .trigger:hover { background: var(--color-surface-2); }
  .swatch {
    width: 10px; height: 10px;
    border-radius: 50%;
    box-shadow: inset 0 0 0 1px rgba(0, 0, 0, 0.15);
  }

  .menu { padding: 10px; display: flex; flex-direction: column; gap: 4px; }
  .section { display: flex; flex-direction: column; gap: 6px; padding: 4px 2px; }
  .label {
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-text-muted);
  }
  .divider { height: 1px; background: var(--color-border); margin: 2px 0; }

  /* Palette list — each row previews its own accent */
  .palette-list { display: flex; flex-direction: column; gap: 2px; }
  .palette-item {
    display: flex;
    align-items: center;
    gap: 10px;
    height: 30px;
    padding: 0 8px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 4px;
    cursor: pointer;
    color: var(--color-text-muted);
    text-align: left;
    font-size: var(--text-xs);
  }
  .palette-item:hover { background: var(--color-surface-2); color: var(--color-text); }
  .palette-item.active {
    background: var(--color-surface-2);
    border-color: var(--color-accent);
    color: var(--color-text);
  }
  .swatch-lg {
    width: 14px; height: 14px;
    border-radius: 50%;
    background: var(--color-accent);
    box-shadow: inset 0 0 0 1px rgba(0, 0, 0, 0.15);
    flex-shrink: 0;
  }
  .palette-label { flex: 1; }
  .check { color: var(--color-accent); flex-shrink: 0; }

  /* Segmented control */
  .seg {
    display: flex;
    gap: 4px;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    padding: 2px;
  }
  .seg-btn {
    flex: 1;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 5px;
    height: 24px;
    padding: 0 8px;
    background: transparent;
    color: var(--color-text-muted);
    border: none;
    border-radius: 3px;
    cursor: pointer;
    text-transform: capitalize;
    font-size: var(--text-xs);
  }
  .seg-btn:hover { background: var(--color-surface-2); }
  .seg-btn.active {
    background: var(--color-accent);
    color: white;
  }
</style>
