import { describe, it, expect } from 'vitest';
import { getOutgoingEdges, getIncomingEdges } from './graph';

function makeNode(id: string): TableNode {
  return { id, table: { name: id, columns: [] }, x: 0, y: 0, w: 100, h: 50 };
}

function makeEdge(sourceId: string, targetId: string): Edge {
  return { source: makeNode(sourceId), target: makeNode(targetId), col: 'fk' };
}

describe('getOutgoingEdges', () => {
  it('returns edges where source matches nodeId', () => {
    const edges = [makeEdge('a', 'b'), makeEdge('a', 'c'), makeEdge('b', 'c')];
    const result = getOutgoingEdges(edges, 'a');
    expect(result).toHaveLength(2);
    expect(result.every(e => e.source.id === 'a')).toBe(true);
  });

  it('returns empty array when no outgoing edges', () => {
    const edges = [makeEdge('a', 'b')];
    expect(getOutgoingEdges(edges, 'b')).toHaveLength(0);
  });

  it('returns empty array for empty edge list', () => {
    expect(getOutgoingEdges([], 'a')).toHaveLength(0);
  });
});

describe('getIncomingEdges', () => {
  it('returns edges where target matches nodeId', () => {
    const edges = [makeEdge('a', 'c'), makeEdge('b', 'c'), makeEdge('c', 'd')];
    const result = getIncomingEdges(edges, 'c');
    expect(result).toHaveLength(2);
    expect(result.every(e => e.target.id === 'c')).toBe(true);
  });

  it('returns empty array when no incoming edges', () => {
    const edges = [makeEdge('a', 'b')];
    expect(getIncomingEdges(edges, 'a')).toHaveLength(0);
  });
});
