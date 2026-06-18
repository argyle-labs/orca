import type { SystemInfoReport } from '$lib/client/types.gen';

export type VersionEntry = {
  tag: string;
  prerelease: boolean;
  published_at: string | null;
  is_current: boolean;
};

export interface Instance {
  id: string;
  peerId: string;
  label: string;
  origin: string;
  port: number;
  role: 'local' | 'system';
  version: string | null;
  target: string | null;
  mode: string | null;
  channel: string | null;
  updateAvailable: boolean;
  updateLatest: string | null;
  updateCheckedSecs: number | null;
  pinnedTo: string | null;
  health: 'up' | 'down' | 'unknown';
  error: string | null;
  lastChecked: number | null;
  secure?: { local: boolean; peer: boolean } | null;
  status?: string | null;
  addresses?: { kind: string; value: string }[] | null;
  sys?: SystemInfoReport | null;
  // Set after a successful peer-dispatched mutation. Polling refreshes
  // (refreshPodPeers) read from the local mesh cache, which lags behind
  // the peer's true state by one mesh sync. While this window is active,
  // preserve fields the action authoritatively changed.
  actionLockUntil?: number;
  // Full version list from this peer's `system.update {}` probe, kept
  // fresh by the page-level fan-out poll. Empty until the first probe
  // completes. The drawer reads from this directly so opening it never
  // needs a Refresh click.
  availableVersions?: VersionEntry[];
}
