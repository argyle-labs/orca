import { describe, it, expect } from 'vitest';
import { computeLayout, edgePath } from './layout';

function makeTable(name: string, cols: string[] = ['id']): Table {
  return { name, columns: cols.map(c => ({ name: c, type: 'int', pk: false, fk: false })) };
}

function makeDomain(key: string, tables: string[], group?: string): Domain {
  return { key, label: key, color: '#aaa', tables, group };
}

describe('computeLayout', () => {
  it('returns empty nodes and edges for empty input', () => {
    const result = computeLayout([], [], []);
    expect(result.nodes).toHaveLength(0);
    expect(result.edges).toHaveLength(0);
  });

  it('produces a node for each table', () => {
    const tables = [makeTable('users'), makeTable('posts')];
    const result = computeLayout(tables, [], []);
    expect(result.nodes).toHaveLength(2);
    expect(result.nodeMap['users']).toBeDefined();
    expect(result.nodeMap['posts']).toBeDefined();
  });

  it('assigns positive x and y positions', () => {
    const tables = [makeTable('users'), makeTable('posts')];
    const result = computeLayout(tables, [], []);
    result.nodes.forEach(n => {
      expect(n.x).toBeGreaterThan(0);
      expect(n.y).toBeGreaterThan(0);
    });
  });

  it('produces an edge for valid FK references', () => {
    const tables = [makeTable('users'), makeTable('posts')];
    const fks: FK[] = [{ from: 'users', to: 'posts', fromCol: 'post_id' }];
    const result = computeLayout(tables, fks, []);
    expect(result.edges).toHaveLength(1);
  });

  it('ignores FK edges that reference missing tables', () => {
    const tables = [makeTable('users')];
    const fks: FK[] = [{ from: 'users', to: 'missing', fromCol: 'x' }];
    const result = computeLayout(tables, fks, []);
    expect(result.edges).toHaveLength(0);
  });

  it('places orphan tables in a _orphan block', () => {
    const tables = [makeTable('orphan')];
    const result = computeLayout(tables, [], []);
    const orphanBlock = result.blocks.find(b => b.domain.key === '_orphan');
    expect(orphanBlock).toBeDefined();
  });

  it('groups tables by domain', () => {
    const tables = [makeTable('users'), makeTable('posts')];
    const domains = [makeDomain('auth', ['users']), makeDomain('content', ['posts'])];
    const result = computeLayout(tables, [], domains);
    expect(result.blocks).toHaveLength(2);
  });

  it('returns wW and wH as positive numbers', () => {
    const tables = [makeTable('users')];
    const result = computeLayout(tables, [], []);
    expect(result.wW).toBeGreaterThan(0);
    expect(result.wH).toBeGreaterThan(0);
  });

  it('builds adjacency lists for edges', () => {
    const tables = [makeTable('users'), makeTable('posts')];
    const fks: FK[] = [{ from: 'users', to: 'posts', fromCol: 'id' }];
    const result = computeLayout(tables, fks, []);
    expect(result.adj['users']).toHaveLength(1);
    expect(result.adj['posts']).toHaveLength(1);
    expect(result.adjT['users'].has('posts')).toBe(true);
    expect(result.adjT['posts'].has('users')).toBe(true);
  });

  it('creates groups for grouped domains', () => {
    const tables = [makeTable('users'), makeTable('sessions')];
    const domains = [
      makeDomain('auth', ['users'], 'core'),
      makeDomain('sessions', ['sessions'], 'core'),
    ];
    const result = computeLayout(tables, [], domains);
    const coreGroup = result.groups.find(g => g.key === 'core');
    expect(coreGroup).toBeDefined();
    expect(coreGroup?.subs).toHaveLength(2);
  });
});

describe('edgePath', () => {
  function makeNode(x: number, y: number, w = 280, h = 100): TableNode {
    return { id: 'n', table: { name: 'n', columns: [] }, x, y, w, h };
  }

  it('returns d string, arrow string, and midpoint numbers', () => {
    const src = makeNode(0, 0);
    const tgt = makeNode(400, 0);
    const result = edgePath(src, tgt);
    expect(typeof result.d).toBe('string');
    expect(typeof result.arrow).toBe('string');
    expect(typeof result.mx).toBe('number');
    expect(typeof result.my).toBe('number');
  });

  it('generates a straight horizontal path when nodes are at same y', () => {
    const src = makeNode(0, 0);
    const tgt = makeNode(400, 0);
    const { d } = edgePath(src, tgt);
    expect(d).toMatch(/^M.*L/);
  });

  it('generates a path when nodes are offset vertically', () => {
    const src = makeNode(0, 0);
    const tgt = makeNode(400, 200);
    const { d } = edgePath(src, tgt);
    expect(d.length).toBeGreaterThan(10);
  });

  it('generates a vertical path when dy > dx', () => {
    const src = makeNode(0, 0);
    const tgt = makeNode(10, 400);
    const { d } = edgePath(src, tgt);
    expect(d.length).toBeGreaterThan(10);
  });

  it('midpoint is between source and target', () => {
    const src = makeNode(0, 0);
    const tgt = makeNode(400, 0);
    const { mx, my } = edgePath(src, tgt);
    expect(mx).toBeGreaterThan(0);
    expect(mx).toBeLessThan(400 + 280);
    expect(typeof my).toBe('number');
  });
});
