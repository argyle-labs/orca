import { cx } from '../utils/utils';
import { ThemeSwitcher } from './ThemeSwitcher';
import type { Palette, Mode } from '../hooks/useTheme';

export function Toolbar({
  title,
  tableCount,
  fkCount,
  searchQuery,
  onSearchChange,
  legendItems,
  activeDomains,
  onToggleDomain,
  palette,
  themeMode,
  onPaletteChange,
  onToggleMode,
  viewMode,
  onViewModeChange,
}: {
  title: string;
  tableCount: number;
  fkCount: number;
  searchQuery: string;
  onSearchChange: (query: string) => void;
  legendItems: LegendItem[];
  activeDomains: Set<string>;
  onToggleDomain: (keys: string[]) => void;
  palette: Palette;
  themeMode: Mode;
  onPaletteChange: (p: Palette) => void;
  onToggleMode: () => void;
  viewMode: string;
  onViewModeChange: (m: 'browse' | 'canvas') => void;
}) {
  return (
    <div id="toolbar">
      <h1>{title}</h1>
      <span className="stats">
        {tableCount} tables &middot; {fkCount} relations
      </span>
      <input
        type="text" id="search" placeholder="Search tables, columns..."
        value={searchQuery}
        onInput={(e) => onSearchChange((e.target as HTMLInputElement).value)}
      />
      <div className="legend">
        {legendItems.map((item) => (
          <div
            key={item.key}
            className={cx('legend-item', !item.groupKeys.some((k) => activeDomains.has(k)) && 'dimmed')}
            onClick={() => onToggleDomain(item.groupKeys)}
          >
            <div className="legend-dot" style={{ background: item.color }} />
            {item.label}
          </div>
        ))}
      </div>
      <div className="schema-mode-toggle">
        <button
          className={cx('mode-btn', viewMode === 'browse' && 'active')}
          onClick={() => onViewModeChange('browse')}
          title="Browse tables"
        >☰</button>
        <button
          className={cx('mode-btn', viewMode === 'canvas' && 'active')}
          onClick={() => onViewModeChange('canvas')}
          title="Canvas view"
        >⊞</button>
      </div>
      <ThemeSwitcher palette={palette} mode={themeMode} onPaletteChange={onPaletteChange} onToggleMode={onToggleMode} />
    </div>
  );
}
