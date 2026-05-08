import { describe, it, expect } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useSearch } from './useSearch';

function makeTable(name: string, cols: { name: string; type: string }[] = []): Table {
  return { name, columns: cols.map(c => ({ ...c, pk: false, fk: false })) };
}

const tables: Table[] = [
  makeTable('users', [{ name: 'email', type: 'varchar' }, { name: 'id', type: 'int' }]),
  makeTable('posts', [{ name: 'title', type: 'varchar' }, { name: 'user_id', type: 'int' }]),
  makeTable('sessions', [{ name: 'token', type: 'text' }]),
];

describe('useSearch', () => {
  it('returns null searchMatchSet when query is empty', () => {
    const { result } = renderHook(() => useSearch(tables));
    expect(result.current.searchMatchSet).toBeNull();
  });

  it('matches table by name (case-insensitive)', () => {
    const { result } = renderHook(() => useSearch(tables));
    act(() => result.current.setSearchQuery('SESSIONS'));
    expect(result.current.searchMatchSet?.has('sessions')).toBe(true);
    expect(result.current.searchMatchSet?.has('posts')).toBe(false);
    expect(result.current.searchMatchSet?.has('users')).toBe(false);
  });

  it('matches table by column name', () => {
    const { result } = renderHook(() => useSearch(tables));
    act(() => result.current.setSearchQuery('user_id'));
    expect(result.current.searchMatchSet?.has('posts')).toBe(true);
  });

  it('matches table by column type', () => {
    const { result } = renderHook(() => useSearch(tables));
    act(() => result.current.setSearchQuery('varchar'));
    expect(result.current.searchMatchSet?.has('users')).toBe(true);
    expect(result.current.searchMatchSet?.has('posts')).toBe(true);
    expect(result.current.searchMatchSet?.has('sessions')).toBe(false);
  });

  it('returns empty set when no tables match', () => {
    const { result } = renderHook(() => useSearch(tables));
    act(() => result.current.setSearchQuery('zzznomatch'));
    expect(result.current.searchMatchSet?.size).toBe(0);
  });

  it('resets to null when query is cleared', () => {
    const { result } = renderHook(() => useSearch(tables));
    act(() => result.current.setSearchQuery('users'));
    act(() => result.current.setSearchQuery(''));
    expect(result.current.searchMatchSet).toBeNull();
  });

  it('exposes searchQuery state', () => {
    const { result } = renderHook(() => useSearch(tables));
    act(() => result.current.setSearchQuery('hello'));
    expect(result.current.searchQuery).toBe('hello');
  });
});

describe('useSearch — regression baseline', () => {
  const realisticTables: Table[] = [
    { name: 'users', columns: [
      { name: 'id', type: 'bigint', pk: true, fk: false },
      { name: 'email', type: 'varchar', pk: false, fk: false },
      { name: 'created_at', type: 'timestamp', pk: false, fk: false },
    ]},
    { name: 'orders', columns: [
      { name: 'id', type: 'bigint', pk: true, fk: false },
      { name: 'user_id', type: 'bigint', pk: false, fk: true },
      { name: 'total', type: 'decimal', pk: false, fk: false },
    ]},
    { name: 'products', columns: [
      { name: 'id', type: 'bigint', pk: true, fk: false },
      { name: 'name', type: 'varchar', pk: false, fk: false },
      { name: 'price', type: 'decimal', pk: false, fk: false },
    ]},
  ];

  it('searching "user" matches users table and orders (via user_id column)', () => {
    const { result } = renderHook(() => useSearch(realisticTables));
    act(() => result.current.setSearchQuery('user'));
    expect(result.current.searchMatchSet?.has('users')).toBe(true);
    expect(result.current.searchMatchSet?.has('orders')).toBe(true);
    expect(result.current.searchMatchSet?.has('products')).toBe(false);
  });

  it('searching "decimal" matches orders and products (type match)', () => {
    const { result } = renderHook(() => useSearch(realisticTables));
    act(() => result.current.setSearchQuery('decimal'));
    expect(result.current.searchMatchSet?.has('orders')).toBe(true);
    expect(result.current.searchMatchSet?.has('products')).toBe(true);
    expect(result.current.searchMatchSet?.has('users')).toBe(false);
  });

  it('searching "bigint" matches all tables', () => {
    const { result } = renderHook(() => useSearch(realisticTables));
    act(() => result.current.setSearchQuery('bigint'));
    expect(result.current.searchMatchSet?.size).toBe(3);
  });
});
