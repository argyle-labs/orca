let _open = $state(false);

export function isCommandPaletteOpen(): boolean {
  return _open;
}

export function openCommandPalette() {
  _open = true;
}

export function closeCommandPalette() {
  _open = false;
}

export function toggleCommandPalette() {
  _open = !_open;
}
