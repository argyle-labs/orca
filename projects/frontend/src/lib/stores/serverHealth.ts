import { writable } from 'svelte/store';

export type ServerStatus = 'unknown' | 'up' | 'down';

const POLL_UP_MS = 10_000;
const POLL_DOWN_MS = 2_000;

function createServerHealth() {
  const { subscribe, set } = writable<ServerStatus>('unknown');
  let timer: ReturnType<typeof setTimeout> | null = null;

  async function check() {
    try {
      const res = await fetch('/api/health', { cache: 'no-store' });
      set(res.ok ? 'up' : 'down');
      timer = setTimeout(check, res.ok ? POLL_UP_MS : POLL_DOWN_MS);
    } catch {
      set('down');
      timer = setTimeout(check, POLL_DOWN_MS);
    }
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
