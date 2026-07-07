import { writable } from 'svelte/store';

export interface Toast {
  id: number;
  message: string;
  type: 'success' | 'error' | 'info';
}

function createNotifications() {
  const { subscribe, update } = writable<Toast[]>([]);
  let next = 0;

  function add(message: string, type: Toast['type'] = 'info', ms = 4000) {
    const id = next++;
    update(t => [...t, { id, message, type }]);
    setTimeout(() => remove(id), ms);
  }

  function remove(id: number) {
    update(t => t.filter(n => n.id !== id));
  }

  return {
    subscribe,
    success: (m: string) => add(m, 'success'),
    error: (m: string) => add(m, 'error', 6000),
    info: (m: string) => add(m, 'info'),
    remove,
  };
}

export const notifications = createNotifications();

/**
 * Unified toast API — same store as `notifications`, cleaner name at the
 * call site. `toast.success('saved')`, `toast.error(err)`, `toast.info(msg)`.
 * Rendered once via `<Notification />` in +layout.svelte.
 */
export const toast = notifications;

/** Wraps an async call with try/catch/notify. Returns the result or null on error. */
export async function act<T>(fn: () => Promise<T>, opts?: { success?: string }): Promise<T | null> {
  try {
    const result = await fn();
    if (opts?.success) notifications.success(opts.success);
    return result;
  } catch (e) {
    notifications.error(String(e));
    return null;
  }
}
