export type Palette = 'violet' | 'ocean' | 'ice-age';
export type Mode = 'dark' | 'light';
export type Theme = Palette;

const PALETTE_KEY = 'orca-palette';
const MODE_KEY = 'orca-mode';

function readAttr<T extends string>(attr: string, valid: T[], fallback: T): T {
  if (typeof document === 'undefined') return fallback;
  const v = document.documentElement.getAttribute(attr) as T | null;
  if (v && valid.includes(v)) return v;
  try {
    const stored = localStorage.getItem(attr === 'data-theme' ? PALETTE_KEY : MODE_KEY) as T | null;
    if (stored && valid.includes(stored)) return stored;
  } catch {
    /* SSR */
  }
  return fallback;
}

function applyTheme(palette: Palette, mode: Mode) {
  if (typeof document === 'undefined') return;
  document.documentElement.setAttribute('data-theme', palette);
  document.documentElement.setAttribute('data-mode', mode);
  try {
    localStorage.setItem(PALETTE_KEY, palette);
    localStorage.setItem(MODE_KEY, mode);
  } catch {
    /* SSR */
  }
}

export function createTheme() {
  let palette = $state<Palette>(
    readAttr('data-theme', ['violet', 'ocean', 'ice-age'] as Palette[], 'violet'),
  );
  let mode = $state<Mode>(readAttr('data-mode', ['dark', 'light'] as Mode[], 'dark'));

  function setPalette(p: Palette) {
    applyTheme(p, mode);
    palette = p;
  }

  function toggleMode() {
    const next: Mode = mode === 'dark' ? 'light' : 'dark';
    applyTheme(palette, next);
    mode = next;
  }

  function startObserver(): () => void {
    if (typeof document === 'undefined') return () => {};
    const observer = new MutationObserver(() => {
      palette = readAttr('data-theme', ['violet', 'ocean', 'ice-age'] as Palette[], 'violet');
      mode = readAttr('data-mode', ['dark', 'light'] as Mode[], 'dark');
    });
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme', 'data-mode'],
    });
    return () => observer.disconnect();
  }

  return {
    get palette() {
      return palette;
    },
    get mode() {
      return mode;
    },
    setPalette,
    toggleMode,
    startObserver,
  };
}
