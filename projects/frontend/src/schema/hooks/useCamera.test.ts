import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook } from '@testing-library/react';
import { useCamera } from './useCamera';

// useCamera depends heavily on DOM event wiring and requestAnimationFrame.
// Unit tests cover: correct interface returned, null-safety for missing viewport.
// Event handler behavior is covered by Playwright E2E tests.

function makeOptions(el?: HTMLDivElement) {
  return {
    viewportRef: { current: el ?? null },
    wW: 2000,
    wH: 1500,
    onBackgroundClick: vi.fn(),
  };
}

describe('useCamera', () => {
  beforeEach(() => {
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { cb(0); return 0; });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('returns expected interface', () => {
    const { result } = renderHook(() => useCamera(makeOptions()));
    expect(typeof result.current.fitAll).toBe('function');
    expect(typeof result.current.focusNode).toBe('function');
    expect(typeof result.current.zoomBy).toBe('function');
    expect(result.current.worldRef).toBeDefined();
    expect(result.current.cam).toBeDefined();
  });

  it('fitAll does not throw when viewport ref is null', () => {
    const { result } = renderHook(() => useCamera(makeOptions()));
    expect(() => result.current.fitAll()).not.toThrow();
  });

  it('zoomBy does not throw when viewport ref is null', () => {
    const { result } = renderHook(() => useCamera(makeOptions()));
    expect(() => result.current.zoomBy(2)).not.toThrow();
  });

  it('focusNode does not throw when viewport ref is null', () => {
    const node: TableNode = { id: 'n', table: { name: 'n', columns: [] }, x: 0, y: 0, w: 100, h: 50 };
    const { result } = renderHook(() => useCamera(makeOptions()));
    expect(() => result.current.focusNode(node)).not.toThrow();
  });

  it('initial cam state is { x:0, y:0, z:1 }', () => {
    const { result } = renderHook(() => useCamera(makeOptions()));
    expect(result.current.cam.current).toEqual({ x: 0, y: 0, z: 1 });
  });
});
