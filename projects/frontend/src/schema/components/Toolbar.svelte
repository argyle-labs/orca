<script lang="ts">
  import { cx } from '../utils/utils';
  import ThemeSwitcher from './ThemeSwitcher.svelte';
  import type { Palette, Mode } from '../hooks/useTheme.svelte';

  interface Props {
    title: string;
    tableCount: number;
    fkCount: number;
    searchQuery: string;
    onsearchchange: (query: string) => void;
    legendItems: LegendItem[];
    activeDomains: Set<string>;
    ontoggledomain: (keys: string[]) => void;
    palette: Palette;
    themeMode: Mode;
    onpalettechange: (p: Palette) => void;
    ontogglemode: () => void;
    viewMode: string;
    onviewmodechange: (m: 'browse' | 'canvas') => void;
  }
  let {
    title, tableCount, fkCount, searchQuery, onsearchchange,
    legendItems, activeDomains, ontoggledomain,
    palette, themeMode, onpalettechange, ontogglemode,
    viewMode, onviewmodechange,
  }: Props = $props();
</script>

<div id="toolbar">
  <h1>{title}</h1>
  <span class="stats">{tableCount} tables &middot; {fkCount} relations</span>
  <input
    type="text"
    id="search"
    placeholder="Search tables, columns..."
    value={searchQuery}
    oninput={(e) => onsearchchange((e.target as HTMLInputElement).value)}
  />
  <div class="legend">
    {#each legendItems as item (item.key)}
      <div
        class={cx('legend-item', !item.groupKeys.some((k) => activeDomains.has(k)) && 'dimmed')}
        onclick={() => ontoggledomain(item.groupKeys)}
        role="button"
        tabindex="0"
        onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') ontoggledomain(item.groupKeys); }}
      >
        <div class="legend-dot" style="background: {item.color}"></div>
        {item.label}
      </div>
    {/each}
  </div>
  <div class="schema-mode-toggle">
    <button
      class={cx('mode-btn', viewMode === 'browse' && 'active')}
      onclick={() => onviewmodechange('browse')}
      title="Browse tables"
    >☰</button>
    <button
      class={cx('mode-btn', viewMode === 'canvas' && 'active')}
      onclick={() => onviewmodechange('canvas')}
      title="Canvas view"
    >⊞</button>
  </div>
  <ThemeSwitcher {palette} mode={themeMode} {onpalettechange} {ontogglemode} />
</div>
