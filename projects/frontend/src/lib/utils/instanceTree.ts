import type { Instance, ClusterSummary, DisplayRow } from '$lib/types/instance';

export interface DisplayInstance {
  inst: Instance;
  depth: number;
  prefix: string;
  hasChildren: boolean;
}

// Depth-first ordering by inferred parent (MAC claim matching). Local host
// always sorts first among roots; remaining roots and children alphabetic
// by hostname. Collapsed nodes short-circuit their subtree.
export function buildDisplayInstances(
  instances: Instance[],
  view: 'tree' | 'table',
  collapsed: Set<string>,
): DisplayInstance[] {
  if (view === 'table') {
    return instances.map(inst => ({ inst, depth: 0, prefix: '', hasChildren: false }));
  }

  const byPeer = new Map<string, Instance>();
  for (const i of instances) byPeer.set(i.peerId, i);

  // Client-side parent inference: match each peer's interface MACs against
  // every other peer's `system.claims[].macs`. Falls back to server-set
  // `parent_peer_id` when present.
  const macIndex = new Map<string, string>();
  for (const inst of instances) {
    for (const c of inst.sys?.claims ?? []) {
      for (const m of c.macs ?? []) {
        if (m) macIndex.set(m.toLowerCase(), inst.peerId);
      }
    }
  }

  function inferParent(inst: Instance): string | null {
    const fromServer = inst.sys?.parent_peer_id;
    if (fromServer && fromServer !== inst.peerId && byPeer.has(fromServer)) return fromServer;
    for (const iface of inst.sys?.interfaces ?? []) {
      if (!iface.mac) continue;
      const claimer = macIndex.get(iface.mac.toLowerCase());
      if (claimer && claimer !== inst.peerId && byPeer.has(claimer)) return claimer;
    }
    return null;
  }

  const childrenOf = new Map<string, Instance[]>();
  const roots: Instance[] = [];
  for (const inst of instances) {
    const parent = inferParent(inst);
    if (parent) {
      const arr = childrenOf.get(parent) ?? [];
      arr.push(inst);
      childrenOf.set(parent, arr);
    } else {
      roots.push(inst);
    }
  }
  for (const arr of childrenOf.values()) {
    arr.sort((a, b) => (a.sys?.hostname ?? a.label).localeCompare(b.sys?.hostname ?? b.label));
  }
  roots.sort((a, b) => {
    if (a.role === 'local') return -1;
    if (b.role === 'local') return 1;
    return (a.sys?.hostname ?? a.label).localeCompare(b.sys?.hostname ?? b.label);
  });

  // Mark every descendant of `parentId` as visited without emitting rows.
  // Used when a parent is collapsed so the orphan-fallback loop below doesn't
  // re-surface hidden children at depth=0 (bug: closing thor used to leave
  // freyr visible as an un-nested root row).
  const visited = new Set<string>();
  function markDescendants(parentId: string) {
    const stack = [parentId];
    while (stack.length) {
      const p = stack.pop()!;
      for (const child of childrenOf.get(p) ?? []) {
        if (!visited.has(child.peerId)) {
          visited.add(child.peerId);
          stack.push(child.peerId);
        }
      }
    }
  }

  const out: DisplayInstance[] = [];
  const walk = (inst: Instance, depth: number, isLastChild: boolean, ancestorLast: boolean[]) => {
    if (visited.has(inst.peerId)) return;
    visited.add(inst.peerId);
    const prefix =
      ancestorLast.map(last => (last ? '   ' : '│  ')).join('') +
      (depth === 0 ? '' : isLastChild ? '└─ ' : '├─ ');
    const kids = childrenOf.get(inst.peerId) ?? [];
    out.push({ inst, depth, prefix, hasChildren: kids.length > 0 });
    if (collapsed.has(inst.peerId)) {
      markDescendants(inst.peerId);
      return;
    }
    for (let i = 0; i < kids.length; i++) {
      walk(kids[i], depth + 1, i === kids.length - 1, [...ancestorLast, isLastChild]);
    }
  };
  for (let i = 0; i < roots.length; i++) {
    walk(roots[i], 0, i === roots.length - 1, []);
  }
  // Surface true orphans (parent_peer_id refers to something we don't have).
  // Hidden-under-collapsed peers are already in `visited` and stay hidden.
  for (const inst of instances) {
    if (!visited.has(inst.peerId)) out.push({ inst, depth: 0, prefix: '', hasChildren: false });
  }
  return out;
}

// Bucket displayInstances by Proxmox cluster. Named clusters alphabetic,
// Ungrouped bucket last. When no clusters exist or view is table, return
// the flat list unwrapped (no headers).
export function buildDisplayRows(
  displayInstances: DisplayInstance[],
  view: 'tree' | 'table',
  clusterByPeer: Map<string, string | null>,
  clusterSummaries: Map<string, ClusterSummary>,
): DisplayRow[] {
  if (view === 'table' || clusterSummaries.size === 0) {
    return displayInstances.map(r => ({
      kind: 'inst' as const,
      inst: r.inst,
      depth: r.depth,
      prefix: r.prefix,
      hasChildren: r.hasChildren,
      key: `i:${r.inst.id}`,
    }));
  }
  const buckets = new Map<string | null, DisplayInstance[]>();
  let currentCluster: string | null = null;
  for (const row of displayInstances) {
    if (row.depth === 0) {
      currentCluster = clusterByPeer.get(row.inst.peerId) ?? null;
    }
    const arr = buckets.get(currentCluster) ?? [];
    arr.push(row);
    buckets.set(currentCluster, arr);
  }
  const named = [...buckets.keys()]
    .filter((k): k is string => k !== null)
    .sort((a, b) => a.localeCompare(b));
  const ordered: (string | null)[] = [...named, ...(buckets.has(null) ? [null] : [])];
  const out: DisplayRow[] = [];
  for (const cname of ordered) {
    const summary = cname ? (clusterSummaries.get(cname) ?? null) : null;
    out.push({
      kind: 'header',
      cluster: cname,
      summary,
      key: `h:${cname ?? '__ungrouped__'}`,
    });
    for (const row of buckets.get(cname) ?? []) {
      out.push({
        kind: 'inst',
        inst: row.inst,
        depth: row.depth,
        prefix: row.prefix,
        hasChildren: row.hasChildren,
        key: `i:${row.inst.id}`,
      });
    }
  }
  return out;
}
