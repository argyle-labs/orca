import { describe, it, expect } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useDomainFilter } from './useDomainFilter';

const domains: Domain[] = [
  { key: 'auth', label: 'Auth', color: '#aaa', tables: ['users'], group: 'core' },
  { key: 'sessions', label: 'Sessions', color: '#bbb', tables: ['sessions'], group: 'core' },
  { key: 'content', label: 'Content', color: '#ccc', tables: ['posts'] },
];

describe('useDomainFilter', () => {
  it('initialises with all domains active', () => {
    const { result } = renderHook(() => useDomainFilter(domains));
    expect(result.current.activeDomains.has('auth')).toBe(true);
    expect(result.current.activeDomains.has('sessions')).toBe(true);
    expect(result.current.activeDomains.has('content')).toBe(true);
  });

  it('toggles a single domain off', () => {
    const { result } = renderHook(() => useDomainFilter(domains));
    act(() => result.current.toggleDomain(['content']));
    expect(result.current.activeDomains.has('content')).toBe(false);
  });

  it('toggles a single domain back on', () => {
    const { result } = renderHook(() => useDomainFilter(domains));
    act(() => result.current.toggleDomain(['content']));
    act(() => result.current.toggleDomain(['content']));
    expect(result.current.activeDomains.has('content')).toBe(true);
  });

  it('toggles a group of domain keys together', () => {
    const { result } = renderHook(() => useDomainFilter(domains));
    act(() => result.current.toggleDomain(['auth', 'sessions']));
    expect(result.current.activeDomains.has('auth')).toBe(false);
    expect(result.current.activeDomains.has('sessions')).toBe(false);
  });

  it('re-activates all group keys when all were inactive', () => {
    const { result } = renderHook(() => useDomainFilter(domains));
    act(() => result.current.toggleDomain(['auth', 'sessions']));
    act(() => result.current.toggleDomain(['auth', 'sessions']));
    expect(result.current.activeDomains.has('auth')).toBe(true);
    expect(result.current.activeDomains.has('sessions')).toBe(true);
  });

  it('deduplicates legend items by group', () => {
    const { result } = renderHook(() => useDomainFilter(domains));
    const coreItem = result.current.legendItems.find(l => l.key === 'core');
    expect(coreItem).toBeDefined();
    expect(coreItem?.groupKeys).toContain('auth');
    expect(coreItem?.groupKeys).toContain('sessions');
  });

  it('creates legend items for ungrouped domains', () => {
    const { result } = renderHook(() => useDomainFilter(domains));
    const contentItem = result.current.legendItems.find(l => l.key === 'content');
    expect(contentItem).toBeDefined();
  });
});
