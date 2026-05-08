import {
  TABLE_WIDTH,
  ROW_HEIGHT,
  HEADER_HEIGHT,
  MAX_VISIBLE_COLS,
  TABLE_GAP,
  DOMAIN_PADDING,
  DOMAIN_GAP,
  DOMAIN_HEADER_HEIGHT,
  GROUP_PADDING,
  GROUP_HEADER_HEIGHT,
  GROUP_SUB_GAP,
} from './constants';

export function computeLayout(tables: Table[], fks: FK[], domains: Domain[]): LayoutResult {
  const domainOf: Record<string, Domain> = {};
  for (const d of domains) {
    for (const t of d.tables) {
      domainOf[t] = d;
    }
  }

  const nodes: TableNode[] = tables.map(table => {
    const visibleRows = Math.min(table.columns.length, MAX_VISIBLE_COLS);
    const overflowIndicator = table.columns.length > MAX_VISIBLE_COLS ? 24 : 0;

    return {
      id: table.name,
      table,
      domain: domainOf[table.name],
      x: 0,
      y: 0,
      w: TABLE_WIDTH,
      h: HEADER_HEIGHT + visibleRows * ROW_HEIGHT + overflowIndicator + 12,
    };
  });

  const nodeMap = Object.fromEntries(nodes.map(n => [n.id, n]));

  const edges: Edge[] = fks
    .map(fk => ({
      source: nodeMap[fk.from],
      target: nodeMap[fk.to],
      col: fk.fromCol,
    }))
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
    .map(domain => ({
      domain,
      nodes: nodes.filter(n => n.domain === domain),
    }))
    .filter(block => block.nodes.length > 0);

  const orphanNodes = nodes.filter(n => !n.domain);
  if (orphanNodes.length > 0) {
    domainBlocks.push({
      domain: { key: '_orphan', label: 'Other', color: '#556', tables: [] as string[] },
      nodes: orphanNodes,
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
      if (!groupedBlocks.has(block.domain.group)) {
        groupedBlocks.set(block.domain.group, []);
      }
      groupedBlocks.get(block.domain.group)!.push(block);
    } else {
      ungroupedBlocks.push(block);
    }
  }

  const groupsFromGrouped: Group[] = [...groupedBlocks.entries()].map(([key, subs]) => {
    let subX = GROUP_PADDING;
    let maxHeight = 0;

    for (const sub of subs) {
      sub.x = subX;
      sub.y = GROUP_PADDING + GROUP_HEADER_HEIGHT;

      for (const node of sub.nodes) {
        node.x += sub.x;
        node.y += sub.y;
      }

      subX += sub.w + GROUP_SUB_GAP;
      maxHeight = Math.max(maxHeight, sub.h);
    }

    return {
      key,
      label: key,
      color: subs[0].domain.color,
      subs,
      w: subX - GROUP_SUB_GAP + GROUP_PADDING,
      h: GROUP_PADDING + GROUP_HEADER_HEIGHT + maxHeight + GROUP_PADDING,
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

  const totalArea = groups.reduce((sum, group) => sum + group.w * group.h, 0);
  const maxRowWidth = Math.max(3000, Math.ceil(Math.sqrt(totalArea)) * 1.6);

  let rowX = 0;
  let rowY = 0;
  let rowMaxHeight = 0;

  for (const group of groups) {
    if (rowX > 0 && rowX + group.w > maxRowWidth) {
      rowX = 0;
      rowY += rowMaxHeight + DOMAIN_GAP;
      rowMaxHeight = 0;
    }

    group.x = rowX;
    group.y = rowY;

    for (const sub of group.subs) {
      const offsetX = group.x + sub.x;
      const offsetY = group.y + sub.y;

      for (const node of sub.nodes) {
        node.x += offsetX - sub.x;
        node.y += offsetY - sub.y;
      }

      sub.x = offsetX;
      sub.y = offsetY;
    }

    rowX += group.w + DOMAIN_GAP;
    rowMaxHeight = Math.max(rowMaxHeight, group.h);
  }

  let [minX, minY, maxX, maxY] = [Infinity, Infinity, -Infinity, -Infinity];
  for (const node of nodes) {
    minX = Math.min(minX, node.x);
    minY = Math.min(minY, node.y);
    maxX = Math.max(maxX, node.x + node.w);
    maxY = Math.max(maxY, node.y + node.h);
  }

  const offsetX = 200 - minX;
  const offsetY = 200 - minY;

  for (const node of nodes) {
    node.x += offsetX;
    node.y += offsetY;
  }
  for (const block of blocks) {
    block.x += offsetX;
    block.y += offsetY;
  }
  for (const group of groups) {
    group.x += offsetX;
    group.y += offsetY;
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

// Orthogonal (rectilinear) routing with rounded corners.
// Paths travel as horizontal + vertical segments so they never arc through
// intermediate boxes and are easy to trace visually.
const CORNER_R = 10;

export function edgePath(source: TableNode, target: TableNode) {
  const srcCX = source.x + source.w / 2;
  const srcCY = source.y + source.h / 2;
  const tgtCX = target.x + target.w / 2;
  const tgtCY = target.y + target.h / 2;

  const dx = tgtCX - srcCX;
  const dy = tgtCY - srcCY;
  const isHorizontal = Math.abs(dx) >= Math.abs(dy);

  // Pick the box edge closest to the target as the exit/entry port.
  const x1 = isHorizontal ? (dx >= 0 ? source.x + source.w : source.x) : srcCX;
  const y1 = isHorizontal ? srcCY : dy >= 0 ? source.y + source.h : source.y;
  const x2 = isHorizontal ? (dx >= 0 ? target.x : target.x + target.w) : tgtCX;
  const y2 = isHorizontal ? tgtCY : dy >= 0 ? target.y : target.y + target.h;

  let d: string;
  let arrowAngle: number;
  let mx: number;
  let my: number;

  if (isHorizontal) {
    const midX = (x1 + x2) / 2;
    const diffY = y2 - y1;

    if (Math.abs(diffY) < 4) {
      // Straight horizontal line — source and target on the same row.
      d = `M${x1},${y1} L${x2},${y2}`;
      mx = midX;
      my = y1;
    } else {
      // Z-shape: ── then │ then ──
      // The vertical segment runs at midX, which sits in the gap between the two boxes.
      const r = Math.min(
        CORNER_R,
        Math.abs(diffY) / 2,
        Math.abs(midX - x1) / 2,
        Math.abs(x2 - midX) / 2,
      );
      const sdx = Math.sign(midX - x1) || 1;
      const sdy = Math.sign(diffY);
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
    const midY = (y1 + y2) / 2;
    const diffX = x2 - x1;

    if (Math.abs(diffX) < 4) {
      // Straight vertical line — source and target in the same column.
      d = `M${x1},${y1} L${x2},${y2}`;
      mx = x1;
      my = midY;
    } else {
      // Z-shape: │ then ── then │
      const r = Math.min(
        CORNER_R,
        Math.abs(diffX) / 2,
        Math.abs(midY - y1) / 2,
        Math.abs(y2 - midY) / 2,
      );
      const sdy = Math.sign(midY - y1) || 1;
      const sdx = Math.sign(diffX);
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

  const arrowLength = 8;
  const arrow = [
    `${x2},${y2}`,
    `${x2 - arrowLength * Math.cos(arrowAngle - 0.35)},${y2 - arrowLength * Math.sin(arrowAngle - 0.35)}`,
    `${x2 - arrowLength * Math.cos(arrowAngle + 0.35)},${y2 - arrowLength * Math.sin(arrowAngle + 0.35)}`,
  ].join(' ');

  return { d, arrow, mx, my };
}
