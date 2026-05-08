import { useState, useEffect, useRef } from 'react';
import { cx } from '../utils/utils';
import type { Palette, Mode } from '../hooks/useTheme';

const PALETTES: { id: Palette; label: string; symbol: string; desc: string }[] = [
  { id: 'violet',  label: 'Violet',  symbol: '◆', desc: 'Dark violet' },
  { id: 'ocean',   label: 'Ocean',   symbol: '🌊', desc: 'Deep ocean blues' },
  { id: 'ice-age', label: 'Ice Age', symbol: '❄',  desc: 'Glacial arctic' },
];

export function ThemeSwitcher({
  palette,
  mode,
  onPaletteChange,
  onToggleMode,
}: {
  palette: Palette;
  mode: Mode;
  onPaletteChange: (p: Palette) => void;
  onToggleMode: () => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const current = PALETTES.find((p) => p.id === palette) ?? PALETTES[0];

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    return () => document.removeEventListener('mousedown', onDown);
  }, [open]);

  return (
    <div className="theme-switcher" ref={ref}>
      <button className={cx('theme-trigger', open && 'open')} onClick={() => setOpen((o) => !o)}>
        <span className="theme-trigger-symbol">{current.symbol}</span>
        <span className="theme-trigger-label">{current.label}</span>
        <span className="theme-trigger-chevron">{open ? '▴' : '▾'}</span>
      </button>

      {open && (
        <div className="theme-dropdown">
          <div className="theme-mode-toggle">
            <button
              className={cx('theme-mode-btn', mode === 'dark' && 'active')}
              onClick={() => { if (mode !== 'dark') onToggleMode(); }}
            >🌑 Dark</button>
            <button
              className={cx('theme-mode-btn', mode === 'light' && 'active')}
              onClick={() => { if (mode !== 'light') onToggleMode(); }}
            >☀ Light</button>
          </div>
          <div className="theme-divider" />
          {PALETTES.map((p) => (
            <button
              key={p.id}
              className={cx('theme-option', palette === p.id && 'active')}
              onClick={() => {
                onPaletteChange(p.id);
                setOpen(false);
              }}
            >
              <span className="theme-option-symbol">{p.symbol}</span>
              <div className="theme-option-info">
                <span className="theme-option-label">{p.label}</span>
                <span className="theme-option-desc">{p.desc}</span>
              </div>
              {palette === p.id && <span className="theme-option-check">✓</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
