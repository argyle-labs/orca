import type {
  PodMember,
  PodPeerDto,
  PodPendingOfferDto,
  SystemDetailResponse,
  SystemUpdateResponse,
} from '$lib/client/types.gen';
import type { Instance, InboundOffer, VersionEntry } from '$lib/types/instance';

export function originForLocal(): string {
  if (typeof window === 'undefined') return '';
  return window.location.origin;
}

export interface InstanceSeedInput {
  peers: { members?: PodMember[] | null };
  probes: Record<string, SystemUpdateResponse>;
  localDetail: SystemDetailResponse | null;
  localHealthy: boolean;
}

function isPodPeerDto(m: PodMember): m is PodPeerDto {
  return 'peer_id' in m && (m as PodPeerDto).status !== undefined;
}

function isPodPendingOfferDto(m: PodMember): m is PodPendingOfferDto {
  return (m as { state?: string }).state === 'handshaking';
}

export function seedInstancesFromLoad(data: InstanceSeedInput): Instance[] {
  const now = Date.now();
  const members: PodMember[] = data.peers.members ?? [];
  const joined = members.filter(isPodPeerDto);
  const selfPeer = joined.find(p => p.local);
  const remotePeers = joined.filter(p => p.status === 'active' && !p.local);

  const localProbe = data.probes['local'];
  const ld = data.localDetail;
  const local: Instance = {
    id: 'local',
    peerId: 'local',
    label: 'local',
    origin: originForLocal(),
    port: 12000,
    role: 'local',
    version: localProbe?.current_version ?? ld?.version ?? null,
    target: ld?.target ?? null,
    mode: ld?.mode ?? null,
    channel: localProbe?.channel ?? ld?.channel ?? null,
    updateAvailable: localProbe?.update_available === true,
    updateLatest: localProbe?.latest ?? null,
    updateCheckedSecs: null,
    pinnedTo: localProbe?.pinned_to ?? ld?.pinned_to ?? null,
    health: data.localHealthy ? 'up' : 'down',
    error: null,
    lastChecked: now,
    sys: ld?.system ?? selfPeer?.system ?? null,
    availableVersions: (localProbe?.available_versions ?? []) as VersionEntry[],
  };

  const podRows: Instance[] = remotePeers.map(p => {
    const probe = data.probes[p.peer_id];
    const version = probe?.current_version ?? p.version ?? null;
    const latest = probe?.latest ?? p.update_latest ?? null;
    return {
      id: `system:${p.peer_id}`,
      peerId: p.peer_id,
      label: p.hostname || p.peer_id,
      origin: `${p.addr}:${p.port}`,
      port: p.port,
      role: 'system' as const,
      version,
      target: p.target ?? null,
      mode: p.mode ?? null,
      channel: probe?.channel ?? p.channel ?? null,
      updateAvailable:
        probe?.update_available === true ||
        (probe?.update_available == null && p.update_available === true),
      updateLatest: latest,
      updateCheckedSecs: p.update_checked_secs ?? null,
      pinnedTo: probe?.pinned_to ?? p.pinned_to ?? null,
      health: p.status === 'active' ? 'up' : 'down',
      error: null,
      lastChecked: now,
      secure: { local: p.local_secure, peer: p.peer_secure },
      status: p.status,
      addresses: (p.addresses ?? []).map(a => ({ kind: a.kind, value: a.value })),
      sys: p.system ?? null,
      availableVersions: (probe?.available_versions ?? []) as VersionEntry[],
    };
  });
  return [local, ...podRows];
}

export interface InboundOfferSeedInput {
  peers: { members?: PodMember[] | null };
}

export function seedInboundOffersFromLoad(data: InboundOfferSeedInput): InboundOffer[] {
  const members: PodMember[] = data.peers.members ?? [];
  const now = Math.floor(Date.now() / 1000);
  return members.filter(isPodPendingOfferDto).filter(r => r.expires_at > now);
}

// Reachable LAN addresses for the card footer. Both IPv4 and IPv6 when
// available. FQDN and Tailscale addrs live in the drawer Addresses section.
export function reachableAddrs(inst: Instance): string[] {
  const port = inst.port;
  const addrs = inst.addresses ?? [];
  const v4 = addrs.find(a => a.kind === 'lan_v4')?.value ?? inst.sys?.primary_ipv4 ?? undefined;
  const v6 = addrs.find(a => a.kind === 'lan_v6')?.value ?? inst.sys?.primary_ipv6 ?? undefined;
  const out: string[] = [];
  if (v4) out.push(`${v4}:${port}`);
  if (v6) out.push(`[${v6}]:${port}`);
  if (out.length > 0) return out;
  if (inst.sys?.fqdn) return [`${inst.sys.fqdn}:${port}`];
  const isIp = /^\d+\.\d+\.\d+\.\d+$|^[0-9a-f:]+$/i.test(inst.label);
  if (!isIp && inst.role !== 'local') return [`${inst.label}:${port}`];
  return [inst.origin];
}
