import init, { OrcaClient } from './orca/index.js';
import { getAuthToken, clearAuthToken } from './stores/authToken.svelte';

let initPromise: Promise<void> | null = null;
let clientPromise: Promise<OrcaClient> | null = null;

function resolveBaseUrl(): string {
  if (typeof window === 'undefined') return '';
  return window.location.origin;
}

/// Build the WASM client once, then re-sync the bearer token on every access
/// so updates from the TokenGate modal take effect immediately.
export function orca(): Promise<OrcaClient> {
  if (!clientPromise) {
    clientPromise = (async () => {
      if (!initPromise) {
        initPromise = init().then(() => undefined);
      }
      await initPromise;
      return new OrcaClient(resolveBaseUrl());
    })();
  }
  return clientPromise.then((c) => {
    const tok = getAuthToken();
    c.setBearerToken(tok ?? '');
    return c;
  });
}

/// Detect a 401 from the WASM client (errors are stringified as
/// "HTTP 401: ..."). Callers should clear the stored token so the TokenGate
/// modal reappears on next render.
export function isUnauthorized(err: unknown): boolean {
  if (!err) return false;
  const msg =
    err instanceof Error ? err.message : typeof err === 'string' ? err : String(err);
  return /HTTP\s+401\b/.test(msg);
}

/// Convenience wrapper: invoke `fn`, and if it 401s, clear the token and
/// rethrow so the caller can surface the error or reload.
export async function withAuth<T>(fn: () => Promise<T>): Promise<T> {
  try {
    return await fn();
  } catch (e) {
    if (isUnauthorized(e)) {
      clearAuthToken();
    }
    throw e;
  }
}
