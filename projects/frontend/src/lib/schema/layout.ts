// Direct port of ice-age's layout.ts + constants

export const TABLE_WIDTH = 280;
export const ROW_HEIGHT = 22;
export const HEADER_HEIGHT = 36;
export const MAX_VISIBLE_COLS = 25;
export const TABLE_GAP = 60;
export const DOMAIN_PADDING = 80;
export const DOMAIN_GAP = 180;
export const DOMAIN_HEADER_HEIGHT = 40;
export const GROUP_PADDING = 50;
export const GROUP_HEADER_HEIGHT = 44;
export const GROUP_SUB_GAP = 60;
export const CORNER_R = 10;

export type Col = { name: string; type: string; pk: boolean; fk: boolean; fkTarget: string | null };
export type Table = { name: string; columns: Col[] };
export type FK = { from: string; to: string; fromCol: string };
export type Domain = {
  key: string;
  label: string;
  color: string;
  tables: string[];
  group?: string;
};

export type TableNode = {
  id: string;
  table: Table;
  domain?: Domain;
  x: number;
  y: number;
  w: number;
  h: number;
};
export type Edge = { source: TableNode; target: TableNode; col: string };
export type Block = {
  domain: Domain;
  nodes: TableNode[];
  x: number;
  y: number;
  w: number;
  h: number;
};
export type Group = {
  key: string;
  label: string;
  color: string;
  subs: Block[];
  x: number;
  y: number;
  w: number;
  h: number;
};

export type LayoutResult = {
  nodes: TableNode[];
  nodeMap: Record<string, TableNode>;
  edges: Edge[];
  blocks: Block[];
  groups: Group[];
  adj: Record<string, Edge[]>;
  adjT: Record<string, Set<string>>;
  wW: number;
  wH: number;
};

export function computeLayout(tables: Table[], fks: FK[], domains: Domain[]): LayoutResult {
  const domainOf: Record<string, Domain> = {};
  for (const d of domains) {
    for (const t of d.tables) domainOf[t] = d;
  }

  const nodes: TableNode[] = tables.map(table => {
    const visibleRows = Math.min(table.columns.length, MAX_VISIBLE_COLS);
    const overflow = table.columns.length > MAX_VISIBLE_COLS ? 24 : 0;
    return {
      id: table.name,
      table,
      domain: domainOf[table.name],
      x: 0,
      y: 0,
      w: TABLE_WIDTH,
      h: HEADER_HEIGHT + visibleRows * ROW_HEIGHT + overflow + 12,
    };
  });

  const nodeMap = Object.fromEntries(nodes.map(n => [n.id, n]));

  const edges: Edge[] = fks
    .map(fk => ({ source: nodeMap[fk.from], target: nodeMap[fk.to], col: fk.fromCol }))
    .filter(e => e.source && e.target);

  function gridLayout(items: TableNode[]) {
    const cols = Math.min(items.length, Math.max(2, Math.ceil(Math.sqrt(items.length * 1.5))));
    const rows = Math.ceil(items.length / cols);
    const rowHeights = Array.from({ length: rows }, (_, row) =>
      Math.max(...Array.from({ length: cols }, (_, col) => items[row * cols + col]?.h ?? 0)),
    );
    let currentY = DOMAIN_PADDING + DOMAIN_HEADER_HEIGHT;
    for (let row = 0; row < rows; row++) {
      for (let col = 0; col < cols; col++) {
        const node = items[row * cols + col];
        if (node) {
          node.x = DOMAIN_PADDING + col * (TABLE_WIDTH + TABLE_GAP);
          node.y = currentY;
        }
      }
      currentY += rowHeights[row] + TABLE_GAP;
    }
    return {
      w: cols * TABLE_WIDTH + (cols - 1) * TABLE_GAP + DOMAIN_PADDING * 2,
      h: currentY - TABLE_GAP + DOMAIN_PADDING,
    };
  }

  const domainBlocks = domains
    .map(domain => ({ domain, nodes: nodes.filter(n => n.domain === domain) }))
    .filter(b => b.nodes.length > 0);

  const orphans = nodes.filter(n => !n.domain);
  if (orphans.length > 0) {
    domainBlocks.push({
      domain: { key: '_other', label: 'Other', color: '#6b7280', tables: [] },
      nodes: orphans,
    });
  }

  const blocks: Block[] = domainBlocks.map(block => ({
    ...block,
    ...gridLayout(block.nodes),
    x: 0,
    y: 0,
  }));

  const groupedBlocks = new Map<string, Block[]>();
  const ungroupedBlocks: Block[] = [];
  for (const block of blocks) {
    if (block.domain.group) {
      if (!groupedBlocks.has(block.domain.group)) groupedBlocks.set(block.domain.group, []);
      groupedBlocks.get(block.domain.group)!.push(block);
    } else {
      ungroupedBlocks.push(block);
    }
  }

  const groupsFromGrouped: Group[] = [...groupedBlocks.entries()].map(([key, subs]) => {
    let subX = GROUP_PADDING;
    let maxH = 0;
    for (const sub of subs) {
      sub.x = subX;
      sub.y = GROUP_PADDING + GROUP_HEADER_HEIGHT;
      for (const node of sub.nodes) {
        node.x += sub.x;
        node.y += sub.y;
      }
      subX += sub.w + GROUP_SUB_GAP;
      maxH = Math.max(maxH, sub.h);
    }
    return {
      key,
      label: key,
      color: subs[0].domain.color,
      subs,
      w: subX - GROUP_SUB_GAP + GROUP_PADDING,
      h: GROUP_PADDING + GROUP_HEADER_HEIGHT + maxH + GROUP_PADDING,
      x: 0,
      y: 0,
    };
  });

  const groupsFromUngrouped: Group[] = ungroupedBlocks.map(block => {
    block.x = 0;
    block.y = 0;
    return {
      key: block.domain.key,
      label: block.domain.label,
      color: block.domain.color,
      subs: [block],
      w: block.w,
      h: block.h,
      x: 0,
      y: 0,
    };
  });

  const groups: Group[] = [...groupsFromGrouped, ...groupsFromUngrouped];

  const totalArea = groups.reduce((s, g) => s + g.w * g.h, 0);
  const maxRowWidth = Math.max(3000, Math.ceil(Math.sqrt(totalArea)) * 1.6);
  let rowX = 0,
    rowY = 0,
    rowMaxH = 0;

  for (const group of groups) {
    if (rowX > 0 && rowX + group.w > maxRowWidth) {
      rowX = 0;
      rowY += rowMaxH + DOMAIN_GAP;
      rowMaxH = 0;
    }
    group.x = rowX;
    group.y = rowY;
    for (const sub of group.subs) {
      const ox = group.x + sub.x,
        oy = group.y + sub.y;
      for (const node of sub.nodes) {
        node.x += ox - sub.x;
        node.y += oy - sub.y;
      }
      sub.x = ox;
      sub.y = oy;
    }
    rowX += group.w + DOMAIN_GAP;
    rowMaxH = Math.max(rowMaxH, group.h);
  }

  let [minX, minY, maxX, maxY] = [Infinity, Infinity, -Infinity, -Infinity];
  for (const node of nodes) {
    minX = Math.min(minX, node.x);
    minY = Math.min(minY, node.y);
    maxX = Math.max(maxX, node.x + node.w);
    maxY = Math.max(maxY, node.y + node.h);
  }
  const ox = 200 - minX,
    oy = 200 - minY;
  for (const node of nodes) {
    node.x += ox;
    node.y += oy;
  }
  for (const block of blocks) {
    block.x += ox;
    block.y += oy;
  }
  for (const group of groups) {
    group.x += ox;
    group.y += oy;
  }

  const adj: Record<string, Edge[]> = {};
  const adjT: Record<string, Set<string>> = {};
  for (const node of nodes) {
    adj[node.id] = [];
    adjT[node.id] = new Set();
  }
  for (const edge of edges) {
    adj[edge.source.id].push(edge);
    adj[edge.target.id].push(edge);
    adjT[edge.source.id].add(edge.target.id);
    adjT[edge.target.id].add(edge.source.id);
  }

  return {
    nodes,
    nodeMap,
    edges,
    blocks,
    groups,
    adj,
    adjT,
    wW: maxX - minX + 400,
    wH: maxY - minY + 400,
  };
}

// Orthogonal (Z-shape) edge routing with rounded corners
export function edgePath(source: TableNode, target: TableNode) {
  const srcCX = source.x + source.w / 2,
    srcCY = source.y + source.h / 2;
  const tgtCX = target.x + target.w / 2,
    tgtCY = target.y + target.h / 2;
  const dx = tgtCX - srcCX,
    dy = tgtCY - srcCY;
  const isH = Math.abs(dx) >= Math.abs(dy);

  const x1 = isH ? (dx >= 0 ? source.x + source.w : source.x) : srcCX;
  const y1 = isH ? srcCY : dy >= 0 ? source.y + source.h : source.y;
  const x2 = isH ? (dx >= 0 ? target.x : target.x + target.w) : tgtCX;
  const y2 = isH ? tgtCY : dy >= 0 ? target.y : target.y + target.h;

  let d: string, arrowAngle: number, mx: number, my: number;

  if (isH) {
    const midX = (x1 + x2) / 2,
      diffY = y2 - y1;
    if (Math.abs(diffY) < 4) {
      d = `M${x1},${y1} L${x2},${y2}`;
      mx = midX;
      my = y1;
    } else {
      const r = Math.min(
        CORNER_R,
        Math.abs(diffY) / 2,
        Math.abs(midX - x1) / 2,
        Math.abs(x2 - midX) / 2,
      );
      const sdx = Math.sign(midX - x1) || 1,
        sdy = Math.sign(diffY);
      d = [
        `M${x1},${y1}`,
        `L${midX - sdx * r},${y1}`,
        `Q${midX},${y1} ${midX},${y1 + sdy * r}`,
        `L${midX},${y2 - sdy * r}`,
        `Q${midX},${y2} ${midX + sdx * r},${y2}`,
        `L${x2},${y2}`,
      ].join(' ');
      mx = midX;
      my = (y1 + y2) / 2;
    }
    arrowAngle = dx >= 0 ? 0 : Math.PI;
  } else {
    const midY = (y1 + y2) / 2,
      diffX = x2 - x1;
    if (Math.abs(diffX) < 4) {
      d = `M${x1},${y1} L${x2},${y2}`;
      mx = x1;
      my = midY;
    } else {
      const r = Math.min(
        CORNER_R,
        Math.abs(diffX) / 2,
        Math.abs(midY - y1) / 2,
        Math.abs(y2 - midY) / 2,
      );
      const sdy = Math.sign(midY - y1) || 1,
        sdx = Math.sign(diffX);
      d = [
        `M${x1},${y1}`,
        `L${x1},${midY - sdy * r}`,
        `Q${x1},${midY} ${x1 + sdx * r},${midY}`,
        `L${x2 - sdx * r},${midY}`,
        `Q${x2},${midY} ${x2},${midY + sdy * r}`,
        `L${x2},${y2}`,
      ].join(' ');
      mx = (x1 + x2) / 2;
      my = midY;
    }
    arrowAngle = dy >= 0 ? Math.PI / 2 : -Math.PI / 2;
  }

  const AL = 8;
  const arrow = [
    `${x2},${y2}`,
    `${x2 - AL * Math.cos(arrowAngle - 0.35)},${y2 - AL * Math.sin(arrowAngle - 0.35)}`,
    `${x2 - AL * Math.cos(arrowAngle + 0.35)},${y2 - AL * Math.sin(arrowAngle + 0.35)}`,
  ].join(' ');

  return { d, arrow, mx, my };
}

function easeInOutQuad(t: number) {
  return t < 0.5 ? 2 * t * t : -1 + (4 - 2 * t) * t;
}

export type Cam = { x: number; y: number; z: number };

export function fitViewParams(vW: number, vH: number, wW: number, wH: number) {
  const z = Math.min(vW / wW, vH / wH, 1) * 0.9;
  return { targetZoom: z, targetX: (wW - vW / z) / 2, targetY: (wH - vH / z) / 2 };
}

export function focusNodeParams(
  vW: number,
  vH: number,
  node: TableNode,
  currentZ: number,
  zoom?: number,
) {
  const targetZoom = zoom ?? Math.max(currentZ, 0.9);
  return {
    targetZoom,
    targetX: node.x + node.w / 2 - vW / (2 * targetZoom),
    targetY: node.y + node.h / 2 - vH / (2 * targetZoom),
  };
}

export function makeAnimateCamera(getCam: () => Cam, applyTransform: () => void) {
  return function animateCamera(
    targetX: number,
    targetY: number,
    targetZoom: number,
    duration = 300,
  ) {
    const cam = getCam();
    const { x: sx, y: sy, z: sz } = cam;
    const startTime = performance.now();
    function step(now: number) {
      const p = Math.min((now - startTime) / duration, 1);
      const e = easeInOutQuad(p);
      cam.x = sx + (targetX - sx) * e;
      cam.y = sy + (targetY - sy) * e;
      cam.z = sz + (targetZoom - sz) * e;
      applyTransform();
      if (p < 1) requestAnimationFrame(step);
    }
    requestAnimationFrame(step);
  };
}
