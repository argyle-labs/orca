import { getPalette, getMode, getFontSize, FONT_SIZES } from '$lib/stores/theme.svelte';

// Mirror the current theme into the <html> element so global CSS rules can
// branch on `[data-theme]` / `[data-mode]` selectors. Reads from the theme
// store; the caller is responsible for invoking from a $effect so changes
// propagate when the store updates.
export function applyThemeToDocument() {
  document.documentElement.setAttribute('data-theme', getPalette());
  document.documentElement.setAttribute('data-mode', getMode());
  const px = FONT_SIZES.find(f => f.id === getFontSize())?.px ?? 15;
  document.documentElement.style.fontSize = `${px}px`;
}
