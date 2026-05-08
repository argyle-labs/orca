<script lang="ts">
  import { cx } from '../utils/utils';
  import type { Palette, Mode } from '../hooks/useTheme.svelte';

  const PALETTES: { id: Palette; label: string; symbol: string; desc: string }[] = [
    { id: 'violet',  label: 'Violet',  symbol: '◆', desc: 'Dark violet' },
    { id: 'ocean',   label: 'Ocean',   symbol: '🌊', desc: 'Deep ocean blues' },
    { id: 'ice-age', label: 'Ice Age', symbol: '❄',  desc: 'Glacial arctic' },
  ];

  interface Props {
    palette: Palette;
    mode: Mode;
    onpalettechange: (p: Palette) => void;
    ontogglemode: () => void;
  }
  let { palette, mode, onpalettechange, ontogglemode }: Props = $props();

  let open = $state(false);
  let ref: HTMLDivElement | undefined = $state();
  const current = $derived(PALETTES.find((p) => p.id === palette) ?? PALETTES[0]);

  $effect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!ref?.contains(e.target as Node)) open = false;
    };
    document.addEventListener('mousedown', onDown);
    return () => document.removeEventListener('mousedown', onDown);
  });
</script>

<div class="theme-switcher" bind:this={ref}>
  <button class={cx('theme-trigger', open && 'open')} onclick={() => (open = !open)}>
    <span class="theme-trigger-symbol">{current.symbol}</span>
    <span class="theme-trigger-label">{current.label}</span>
    <span class="theme-trigger-chevron">{open ? '▴' : '▾'}</span>
  </button>

  {#if open}
    <div class="theme-dropdown">
      <div class="theme-mode-toggle">
        <button
          class={cx('theme-mode-btn', mode === 'dark' && 'active')}
          onclick={() => { if (mode !== 'dark') ontogglemode(); }}
        >🌑 Dark</button>
        <button
          class={cx('theme-mode-btn', mode === 'light' && 'active')}
          onclick={() => { if (mode !== 'light') ontogglemode(); }}
        >☀ Light</button>
      </div>
      <div class="theme-divider"></div>
      {#each PALETTES as p (p.id)}
        <button
          class={cx('theme-option', palette === p.id && 'active')}
          onclick={() => { onpalettechange(p.id); open = false; }}
        >
          <span class="theme-option-symbol">{p.symbol}</span>
          <div class="theme-option-info">
            <span class="theme-option-label">{p.label}</span>
            <span class="theme-option-desc">{p.desc}</span>
          </div>
          {#if palette === p.id}<span class="theme-option-check">✓</span>{/if}
        </button>
      {/each}
    </div>
  {/if}
</div>
