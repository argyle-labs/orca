import init, { OrcaClient } from './orca/index.js';

let initPromise: Promise<void> | null = null;
let clientPromise: Promise<OrcaClient> | null = null;

function resolveBaseUrl(): string {
  if (typeof window === 'undefined') return '';
  return window.location.origin;
}

export function orca(): Promise<OrcaClient> {
  if (clientPromise) return clientPromise;
  clientPromise = (async () => {
    if (!initPromise) {
      initPromise = init().then(() => undefined);
    }
    await initPromise;
    return new OrcaClient(resolveBaseUrl());
  })();
  return clientPromise;
}
