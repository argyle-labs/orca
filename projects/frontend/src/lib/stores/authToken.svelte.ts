// Bearer token for the orca REST API.
//
// Stored in localStorage so it survives reloads. The orcaClient module reads
// it on every `orca()` call and attaches it to outgoing requests. On HTTP
// 401, callers should invoke `clearAuthToken()` and the TokenGate modal in
// the root layout will reappear.

const STORAGE_KEY = 'orca_token';

function readInitial(): string | null {
  if (typeof window === 'undefined') return null;
  try {
    const v = window.localStorage.getItem(STORAGE_KEY);
    return v && v.length > 0 ? v : null;
  } catch {
    return null;
  }
}

let token = $state<string | null>(readInitial());

export function getAuthToken(): string | null {
  return token;
}

export function setAuthToken(next: string): void {
  token = next;
  try {
    window.localStorage.setItem(STORAGE_KEY, next);
  } catch {
    // localStorage disabled — token lives only in memory for this session.
  }
}

export function clearAuthToken(): void {
  token = null;
  try {
    window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    // Ignore — clearing in-memory state is enough.
  }
}

/// Reactive accessor for components that want to re-render when the token
/// flips (e.g. TokenGate). Always returns the current value.
export function authTokenSnapshot(): string | null {
  return token;
}
