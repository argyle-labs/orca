export type Palette = 'violet' | 'ocean' | 'ice-age' | 'forest' | 'sunset' | 'rose' | 'mono';
export type Mode = 'dark' | 'light';
export type FontSize = 'sm' | 'md' | 'lg';

export const PALETTES: { id: Palette; label: string; symbol: string }[] = [
  { id: 'violet', label: 'Violet', symbol: '◆' },
  { id: 'ocean', label: 'Ocean', symbol: '~' },
  { id: 'ice-age', label: 'Ice Age', symbol: '❄' },
  { id: 'forest', label: 'Forest', symbol: '♣' },
  { id: 'sunset', label: 'Sunset', symbol: '☀' },
  { id: 'rose', label: 'Rose', symbol: '✿' },
  { id: 'mono', label: 'Mono', symbol: '●' },
];

export const FONT_SIZES: { id: FontSize; label: string; symbol: string; px: number }[] = [
  { id: 'sm', label: 'Small', symbol: 'a', px: 13 },
  { id: 'md', label: 'Default', symbol: 'A', px: 15 },
  { id: 'lg', label: 'Large', symbol: 'A+', px: 17 },
];

const PALETTE_KEY = 'orca-palette';
const MODE_KEY = 'orca-mode';
const FONT_SIZE_KEY = 'orca-font-size';

const PALETTE_IDS: Palette[] = ['violet', 'ocean', 'ice-age', 'forest', 'sunset', 'rose', 'mono'];

function readPalette(): Palette {
  try {
    const v = localStorage.getItem(PALETTE_KEY);
    if (v && (PALETTE_IDS as string[]).includes(v)) return v as Palette;
  } catch {
    /* SSR */
  }
  return 'violet';
}

function readMode(): Mode {
  try {
    const v = localStorage.getItem(MODE_KEY);
    if (v === 'dark' || v === 'light') return v;
  } catch {
    /* SSR */
  }
  return 'dark';
}

function readFontSize(): FontSize {
  try {
    const v = localStorage.getItem(FONT_SIZE_KEY);
    if (v === 'sm' || v === 'md' || v === 'lg') return v;
  } catch {
    /* SSR */
  }
  return 'md';
}

let _palette = $state<Palette>(readPalette());
let _mode = $state<Mode>(readMode());
let _fontSize = $state<FontSize>(readFontSize());

export function getPalette(): Palette {
  return _palette;
}
export function getMode(): Mode {
  return _mode;
}
export function getFontSize(): FontSize {
  return _fontSize;
}

export function setPalette(p: Palette) {
  _palette = p;
  try {
    localStorage.setItem(PALETTE_KEY, p);
  } catch {
    /* SSR */
  }
}

export function setMode(m: Mode) {
  _mode = m;
  try {
    localStorage.setItem(MODE_KEY, m);
  } catch {
    /* SSR */
  }
}

export function toggleMode() {
  setMode(_mode === 'dark' ? 'light' : 'dark');
}

export function setFontSize(s: FontSize) {
  _fontSize = s;
  try {
    localStorage.setItem(FONT_SIZE_KEY, s);
  } catch {
    /* SSR */
  }
}
