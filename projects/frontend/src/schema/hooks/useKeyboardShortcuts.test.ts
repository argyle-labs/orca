import { describe, it, expect, vi } from 'vitest';
import { renderHook } from '@testing-library/react';
import { useKeyboardShortcuts } from './useKeyboardShortcuts';

function fireKey(key: string) {
  const event = new KeyboardEvent('keydown', { key, bubbles: true });
  window.dispatchEvent(event);
}

describe('useKeyboardShortcuts', () => {
  it('calls onClear when Escape is pressed', () => {
    const onClear = vi.fn();
    renderHook(() => useKeyboardShortcuts({ onClear }));
    fireKey('Escape');
    expect(onClear).toHaveBeenCalledTimes(1);
  });

  it('focuses #search element when / is pressed', () => {
    const searchEl = document.createElement('input');
    searchEl.id = 'search';
    document.body.appendChild(searchEl);
    const focusSpy = vi.spyOn(searchEl, 'focus');

    const onClear = vi.fn();
    renderHook(() => useKeyboardShortcuts({ onClear }));
    fireKey('/');
    expect(focusSpy).toHaveBeenCalled();

    document.body.removeChild(searchEl);
  });

  it('removes event listener on unmount', () => {
    const onClear = vi.fn();
    const { unmount } = renderHook(() => useKeyboardShortcuts({ onClear }));
    unmount();
    fireKey('Escape');
    expect(onClear).not.toHaveBeenCalled();
  });
});
