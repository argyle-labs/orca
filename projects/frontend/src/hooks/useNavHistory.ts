import { useCallback } from 'react';

const KEY = 'orca-nav-history';
const MAX = 50;

export interface NavEntry {
  path: string;
  title: string;
  ts: number;
}

function read(): NavEntry[] {
  try { return JSON.parse(localStorage.getItem(KEY) ?? '[]'); } catch { return []; }
}

export function useNavHistory() {
  const record = useCallback((path: string, title: string) => {
    const entries = read().filter((e) => e.path !== path);
    entries.unshift({ path, title, ts: Date.now() });
    try { localStorage.setItem(KEY, JSON.stringify(entries.slice(0, MAX))); } catch {}
  }, []);

  const getRecent = useCallback((n = 10): NavEntry[] => read().slice(0, n), []);

  return { record, getRecent };
}

export function timeAgo(ts: number): string {
  const s = Math.floor((Date.now() - ts) / 1000);
  if (s < 60) return 'just now';
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  return `${d}d ago`;
}
