import { describe, it, expect, vi, afterEach } from 'vitest';

describe('staleMs', () => {
  afterEach(() => {
    vi.unstubAllEnvs();
    vi.resetModules();
  });

  it('returns 0 in dev mode regardless of input', async () => {
    vi.stubEnv('DEV', true);
    const { staleMs } = await import('./stale');
    expect(staleMs(60_000)).toBe(0);
    expect(staleMs(0)).toBe(0);
    expect(staleMs(300_000)).toBe(0);
  });

  it('returns the provided ms in production mode', async () => {
    vi.stubEnv('DEV', false);
    const { staleMs } = await import('./stale');
    expect(staleMs(60_000)).toBe(60_000);
    expect(staleMs(300_000)).toBe(300_000);
  });

  it('returns 0 for 0ms in production', async () => {
    vi.stubEnv('DEV', false);
    const { staleMs } = await import('./stale');
    expect(staleMs(0)).toBe(0);
  });
});
