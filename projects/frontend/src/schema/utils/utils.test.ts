import { describe, it, expect } from 'vitest';
import { cx, buildDomainMap } from './utils';

describe('cx', () => {
  it('joins truthy strings', () => {
    expect(cx('a', 'b', 'c')).toBe('a b c');
  });

  it('filters falsy values', () => {
    expect(cx('a', false, null, undefined, 'b')).toBe('a b');
  });

  it('returns empty string for all falsy', () => {
    expect(cx(false, null, undefined)).toBe('');
  });

  it('handles single class', () => {
    expect(cx('only')).toBe('only');
  });
});

describe('buildDomainMap', () => {
  it('maps each table name to its domain', () => {
    const domains: Domain[] = [
      { key: 'a', label: 'A', color: '#f00', tables: ['orders', 'items'] },
      { key: 'b', label: 'B', color: '#0f0', tables: ['users'] },
    ];
    const map = buildDomainMap(domains);
    expect(map['orders'].key).toBe('a');
    expect(map['items'].key).toBe('a');
    expect(map['users'].key).toBe('b');
  });

  it('last domain wins for duplicate table entries', () => {
    const domains: Domain[] = [
      { key: 'first', label: 'F', color: '#f00', tables: ['shared'] },
      { key: 'last', label: 'L', color: '#0f0', tables: ['shared'] },
    ];
    const map = buildDomainMap(domains);
    expect(map['shared'].key).toBe('last');
  });

  it('returns empty map for no domains', () => {
    expect(buildDomainMap([])).toEqual({});
  });

  it('returns empty map for domains with no tables', () => {
    const domains: Domain[] = [
      { key: 'empty', label: 'E', color: '#000', tables: [] },
    ];
    expect(buildDomainMap(domains)).toEqual({});
  });
});
