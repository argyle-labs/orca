import { writable } from 'svelte/store';
import { orca } from '$lib/orcaClient';

export type ServerStatus = 'unknown' | 'up' | 'down';

const POLL_UP_MS = 10_000;
const POLL_DOWN_MS = 2_000;

function createServerHealth() {
  const { subscribe, set } = writable<ServerStatus>('unknown');
  let timer: ReturnType<typeof setTimeout> | null = null;

  async function check() {
    let ok: boolean;
    try {
      const client = await orca();
      const result = (await client.health({})) as { ok: boolean };
      ok = !!result?.ok;
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
