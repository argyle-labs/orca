import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useNavHistory, timeAgo } from './useNavHistory';

beforeEach(() => localStorage.clear());

describe('useNavHistory', () => {
  it('record() adds an entry and getRecent() returns it', () => {
    const { result } = renderHook(() => useNavHistory());
    act(() => { result.current.record('/foo', 'Foo Page'); });
    const entries = result.current.getRecent();
    expect(entries).toHaveLength(1);
    expect(entries[0].path).toBe('/foo');
    expect(entries[0].title).toBe('Foo Page');
    expect(typeof entries[0].ts).toBe('number');
  });

  it('deduplicates: re-recording the same path moves it to front with updated title', () => {
    const { result } = renderHook(() => useNavHistory());
    act(() => {
      result.current.record('/a', 'A');
      result.current.record('/b', 'B');
      result.current.record('/a', 'A Updated');
    });
    const entries = result.current.getRecent();
    expect(entries[0].path).toBe('/a');
    expect(entries[0].title).toBe('A Updated');
    expect(entries).toHaveLength(2);
  });

  it('most recently recorded path is first', () => {
    const { result } = renderHook(() => useNavHistory());
    act(() => {
      result.current.record('/first', 'First');
      result.current.record('/second', 'Second');
      result.current.record('/third', 'Third');
    });
    const entries = result.current.getRecent();
    expect(entries[0].path).toBe('/third');
    expect(entries[1].path).toBe('/second');
    expect(entries[2].path).toBe('/first');
  });

  it('getRecent(n) limits to n entries', () => {
    const { result } = renderHook(() => useNavHistory());
    act(() => {
      for (let i = 0; i < 10; i++) result.current.record(`/page-${i}`, `Page ${i}`);
    });
    expect(result.current.getRecent(3)).toHaveLength(3);
  });

  it('caps storage at 50 entries', () => {
    const { result } = renderHook(() => useNavHistory());
    act(() => {
      for (let i = 0; i < 60; i++) result.current.record(`/page-${i}`, `Page ${i}`);
    });
    expect(result.current.getRecent(100)).toHaveLength(50);
  });

  it('returns empty array when localStorage is empty', () => {
    const { result } = renderHook(() => useNavHistory());
    expect(result.current.getRecent()).toEqual([]);
  });
});

describe('timeAgo', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2024-01-01T12:00:00Z'));
  });

  afterEach(() => { vi.useRealTimers(); });

  it('returns "just now" for times under 60 seconds ago', () => {
    expect(timeAgo(Date.now() - 30_000)).toBe('just now');
  });

  it('returns "Xm ago" for times in the last hour', () => {
    expect(timeAgo(Date.now() - 5 * 60_000)).toBe('5m ago');
  });

  it('returns "Xh ago" for times in the last day', () => {
    expect(timeAgo(Date.now() - 3 * 3_600_000)).toBe('3h ago');
  });

  it('returns "Xd ago" for times over a day old', () => {
    expect(timeAgo(Date.now() - 2 * 86_400_000)).toBe('2d ago');
  });
});
