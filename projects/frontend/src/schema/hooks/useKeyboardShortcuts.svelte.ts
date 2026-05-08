interface Options {
  onClear: () => void;
}

export function attachKeyboardShortcuts({ onClear }: Options): () => void {
  if (typeof window === 'undefined') return () => {};
  const handleKeyDown = (ev: KeyboardEvent) => {
    if (ev.key === 'Escape') onClear();
    if (ev.key === '/' && (document.activeElement as HTMLElement)?.id !== 'search') {
      ev.preventDefault();
      document.getElementById('search')?.focus();
    }
  };
  window.addEventListener('keydown', handleKeyDown);
  return () => window.removeEventListener('keydown', handleKeyDown);
}
