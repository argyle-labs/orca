// Single polling primitive. Every recurring fetch in the UI flows through
// here so cadence, immediate-fire, and teardown are handled the same way
// — no more hand-rolled `setInterval` + `onDestroy` pairs sprinkled across
// pages and components.
//
// Usage:
//   const poller = createPoller({ fn: refreshThing, intervalMs: 5000 });
//   onMount(poller.start);
//   onDestroy(poller.stop);
//
// `immediate: false` skips the initial fire (default fires immediately so
// the caller doesn't need a separate kick-off line). `fn` may be sync or
// async; overlapping ticks are dropped — if a previous run hasn't resolved
// when the next interval fires, that tick is skipped rather than queued.

export interface PollerOptions {
  fn: () => void | Promise<void>;
  intervalMs: number;
  immediate?: boolean;
}

export interface Poller {
  start: () => void;
  stop: () => void;
}

export function createPoller(opts: PollerOptions): Poller {
  const { fn, intervalMs, immediate = true } = opts;
  let handle: ReturnType<typeof setInterval> | null = null;
  let inFlight = false;

  const tick = async () => {
    if (inFlight) return;
    inFlight = true;
    try {
      await fn();
    } finally {
      inFlight = false;
    }
  };

  return {
    start() {
      if (handle) return;
      if (immediate) void tick();
      handle = setInterval(tick, intervalMs);
    },
    stop() {
      if (handle) {
        clearInterval(handle);
        handle = null;
      }
    },
  };
}
