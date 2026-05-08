import { browser } from '$app/environment';

const KEY = 'orca-nav-history';
const MAX = 20;

export interface NavEntry { path: string; ts: number; }

function load(): NavEntry[] {
  if (!browser) return [];
  try { return JSON.parse(localStorage.getItem(KEY) ?? '[]'); } catch { return []; }
}

function save(entries: NavEntry[]): void {
  if (browser) localStorage.setItem(KEY, JSON.stringify(entries));
}

export function recordNav(path: string): void {
  const entries = load().filter(e => e.path !== path);
  entries.unshift({ path, ts: Date.now() });
  save(entries.slice(0, MAX));
}

export function getRecent(n: number): NavEntry[] {
  return load().slice(0, n);
}

export function timeAgo(ts: number): string {
  const s = Math.floor((Date.now() - ts) / 1000);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}
