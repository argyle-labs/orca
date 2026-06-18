import type { SystemInfoReport } from '$lib/client/types.gen';

export type InboundOffer = {
  offer_id: string;
  peer_hostname: string;
  peer_addr: string;
  peer_port: number;
  inviter_peer_id?: string | null;
  expires_at: number;
  ttl_secs: number;
};

export type Candidate = {
  pubkey_fp: string;
  peer_id: string | null;
  hostname: string;
  addr: string;
  port: number;
  can_invite: boolean;
};

export type StaleRow = {
  peer_id: string;
  hostname: string;
  addr: string;
  port: number;
  reason: string;
  last_seen_at: number | null;
};

export type ClusterSummary = {
  name: string;
  quorate: boolean | null;
  online: number;
  total: number;
};

export type DisplayRow =
  | { kind: 'header'; cluster: string | null; summary: ClusterSummary | null; key: string }
  | {
      kind: 'inst';
      inst: Instance;
      depth: number;
      prefix: string;
      hasChildren: boolean;
      key: string;
    };

export type SystemUpdateResp = {
  current_version: string;
  channel: string;
  pinned_to: string | null;
  available_versions: VersionEntry[];
  latest: string | null;
  notes: string[];
  errors: string[];
  update_available?: boolean | null;
};

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
