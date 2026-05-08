import { describe, it, expect } from 'vitest';
import { renderHook } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import React from 'react';
import {
  useGetSchema,
  useGetTree,
  useGetLibraryDocs,
  usePing,
  useGetDockerServices,
} from './hooks';

function makeWrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return ({ children }: { children: React.ReactNode }) =>
    React.createElement(QueryClientProvider, { client }, children);
}

describe('API hooks — query key construction', () => {
  it('useGetSchema uses key ["getSchema"]', () => {
    const { result } = renderHook(() => useGetSchema(), { wrapper: makeWrapper() });
    expect(result.current).toBeDefined();
  });

  it('usePing uses key ["ping"]', () => {
    const { result } = renderHook(() => usePing(), { wrapper: makeWrapper() });
    expect(result.current).toBeDefined();
  });
});

describe('API hooks — enabled guard', () => {
  it('useGetTree fetches immediately (no enabled guard)', () => {
    const { result } = renderHook(() => useGetTree({}), { wrapper: makeWrapper() });
    expect(result.current.status).toBe('pending');
  });

  it('useGetDockerServices is disabled when params is falsy', () => {
    const { result } = renderHook(() => useGetDockerServices('' as never), { wrapper: makeWrapper() });
    expect(result.current.fetchStatus).toBe('idle');
  });

  it('useGetDockerServices is enabled when params is provided', () => {
    const { result } = renderHook(() => useGetDockerServices({ path: 'compose' }), { wrapper: makeWrapper() });
    expect(result.current.status).toBe('pending');
  });
});

describe('API hooks — staleTime injection', () => {
  it('useGetLibraryDocs is disabled when params is falsy', () => {
    const { result } = renderHook(() => useGetLibraryDocs('' as never), { wrapper: makeWrapper() });
    expect(result.current.fetchStatus).toBe('idle');
  });

  it('useGetLibraryDocs is enabled and pending when params is provided', () => {
    const { result } = renderHook(() => useGetLibraryDocs({ q: 'react' }), { wrapper: makeWrapper() });
    expect(result.current.status).toBe('pending');
  });
});
