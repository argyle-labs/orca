import { writable } from 'svelte/store';

export type ServerStatus = 'unknown' | 'up' | 'down';

const POLL_UP_MS = 10_000;
const POLL_DOWN_MS = 2_000;

function createServerHealth() {
  const { subscribe, set } = writable<ServerStatus>('unknown');
  let timer: ReturnType<typeof setTimeout> | null = null;

  async function check() {
    // /api/health is open (no auth). Any HTTP response — even a non-2xx —
    // proves reachability. Only a thrown fetch error means "down". A 401
    // elsewhere is "not signed in", NOT a backend outage.
    let ok: boolean;
    try {
      const res = await fetch('/api/health', { credentials: 'include' });
      ok = !!res;
    } catch {
      ok = false;
    }
    set(ok ? 'up' : 'down');
    timer = setTimeout(check, ok ? POLL_UP_MS : POLL_DOWN_MS);
  }

  function start() {
    check();
    return () => {
      if (timer) clearTimeout(timer);
    };
  }

  function retry() {
    if (timer) clearTimeout(timer);
    check();
  }

  return { subscribe, start, retry };
}

export const serverHealth = createServerHealth();
