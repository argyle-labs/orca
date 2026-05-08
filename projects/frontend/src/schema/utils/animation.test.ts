import { describe, it, expect } from 'vitest';
import { easeInOutQuad, clampZoom } from './animation';

describe('easeInOutQuad', () => {
  it('returns 0 at progress=0', () => {
    expect(easeInOutQuad(0)).toBe(0);
  });

  it('returns 1 at progress=1', () => {
    expect(easeInOutQuad(1)).toBe(1);
  });

  it('returns 0.5 at midpoint', () => {
    expect(easeInOutQuad(0.5)).toBe(0.5);
  });

  it('is symmetric around 0.5', () => {
    const a = easeInOutQuad(0.25);
    const b = 1 - easeInOutQuad(0.75);
    expect(a).toBeCloseTo(b, 10);
  });

  it('is monotonically increasing', () => {
    const values = [0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1].map(easeInOutQuad);
    for (let i = 1; i < values.length; i++) {
      expect(values[i]).toBeGreaterThanOrEqual(values[i - 1]);
    }
  });
});

describe('clampZoom', () => {
  it('clamps below min to 0.05', () => {
    expect(clampZoom(0)).toBe(0.05);
    expect(clampZoom(-1)).toBe(0.05);
  });

  it('clamps above max to 5', () => {
    expect(clampZoom(10)).toBe(5);
    expect(clampZoom(999)).toBe(5);
  });

  it('passes through values in range', () => {
    expect(clampZoom(1)).toBe(1);
    expect(clampZoom(0.5)).toBe(0.5);
    expect(clampZoom(3)).toBe(3);
  });

  it('respects custom min/max', () => {
    expect(clampZoom(0.1, 0.2, 2)).toBe(0.2);
    expect(clampZoom(3, 0.2, 2)).toBe(2);
  });
});
