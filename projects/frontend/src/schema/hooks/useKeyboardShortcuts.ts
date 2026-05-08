import { useEffect } from 'react';

interface UseKeyboardShortcutsOptions {
  onClear: () => void;
}

export function useKeyboardShortcuts({ onClear }: UseKeyboardShortcutsOptions) {
  useEffect(() => {
    const handleKeyDown = (ev: KeyboardEvent) => {
      if (ev.key === 'Escape') {
        onClear();
      }
      if (ev.key === '/' && (document.activeElement as HTMLElement)?.id !== 'search') {
        ev.preventDefault();
        document.getElementById('search')?.focus();
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [onClear]);
}
