import { useState, useEffect, useCallback } from 'react';

export type Palette = 'violet' | 'ocean' | 'ice-age';
export type Mode    = 'dark' | 'light';
export type Theme   = Palette; // alias used by ThemeSwitcher

const PALETTE_KEY = 'orca-palette';
const MODE_KEY    = 'orca-mode';

function readAttr<T extends string>(attr: string, valid: T[], fallback: T): T {
  const v = document.documentElement.getAttribute(attr) as T | null;
  if (v && valid.includes(v)) return v;
  try {
    const stored = localStorage.getItem(attr === 'data-theme' ? PALETTE_KEY : MODE_KEY) as T | null;
    if (stored && valid.includes(stored)) return stored;
  } catch { /* SSR */ }
  return fallback;
}

function applyTheme(palette: Palette, mode: Mode) {
  document.documentElement.setAttribute('data-theme', palette);
  document.documentElement.setAttribute('data-mode', mode);
  try {
    localStorage.setItem(PALETTE_KEY, palette);
    localStorage.setItem(MODE_KEY, mode);
  } catch { /* SSR */ }
}

export function useTheme(): {
  palette: Palette;
  mode: Mode;
  setPalette: (p: Palette) => void;
  toggleMode: () => void;
} {
  const [palette, setPaletteState] = useState<Palette>(() =>
    readAttr('data-theme', ['violet', 'ocean', 'ice-age'] as Palette[], 'violet')
  );
  const [mode, setModeState] = useState<Mode>(() =>
    readAttr('data-mode', ['dark', 'light'] as Mode[], 'dark')
  );

  const setPalette = useCallback((p: Palette) => {
    applyTheme(p, mode);
    setPaletteState(p);
  }, [mode]);

  const toggleMode = useCallback(() => {
    const next: Mode = mode === 'dark' ? 'light' : 'dark';
    applyTheme(palette, next);
    setModeState(next);
  }, [palette, mode]);

  useEffect(() => {
    const observer = new MutationObserver(() => {
      setPaletteState(readAttr('data-theme', ['violet', 'ocean', 'ice-age'] as Palette[], 'violet'));
      setModeState(readAttr('data-mode', ['dark', 'light'] as Mode[], 'dark'));
    });
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme', 'data-mode'],
    });
    return () => observer.disconnect();
  }, []);

  return { palette, mode, setPalette, toggleMode };
}
