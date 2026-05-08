import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

// Set up global location mock before importing the module
Object.defineProperty(window, 'location', {
  value: { origin: 'http://localhost:12001' },
  writable: true,
});

describe('API client', () => {
  const originalFetch = globalThis.fetch;

  beforeEach(() => {
    globalThis.fetch = vi.fn();
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
    vi.clearAllMocks();
  });

  function mockFetch(status: number, body: unknown, contentType = 'application/json') {
    const bodyStr = typeof body === 'string' ? body : JSON.stringify(body);
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      ok: status >= 200 && status < 300,
      status,
      statusText: status === 200 ? 'OK' : 'Error',
      headers: { get: () => contentType },
      json: () => Promise.resolve(body),
      text: () => Promise.resolve(bodyStr),
    });
  }

  it('returns JSON for application/json responses', async () => {
    const { getTree } = await import('./client');
    mockFetch(200, { nodes: [] });
    const result = await getTree({});
    expect(result).toEqual({ nodes: [] });
  });

  it('returns text for text/plain responses', async () => {
    const { getDoc } = await import('./client');
    mockFetch(200, '# My Doc\n\nContent here', 'text/plain; charset=utf-8');
    const result = await getDoc({ root: 'orca', path: 'notes/test' });
    expect(typeof result).toBe('string');
  });

  it('throws on non-ok responses with JSON error body', async () => {
    const { getTree } = await import('./client');
    mockFetch(500, { error: 'internal server error' });
    await expect(getTree({})).rejects.toThrow('internal server error');
  });

  it('throws with statusText when error body is not JSON', async () => {
    const { getTree } = await import('./client');
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      ok: false,
      status: 503,
      statusText: 'Service Unavailable',
      headers: { get: () => 'text/html' },
      json: () => Promise.reject(new Error('not json')),
      text: () => Promise.resolve('<html>error</html>'),
    });
    await expect(getTree({})).rejects.toThrow('Service Unavailable');
  });
});
