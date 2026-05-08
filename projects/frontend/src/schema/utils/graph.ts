export function getOutgoingEdges(edges: Edge[], nodeId: string): Edge[] {
  return edges.filter(e => e.source.id === nodeId);
}

export function getIncomingEdges(edges: Edge[], nodeId: string): Edge[] {
  return edges.filter(e => e.target.id === nodeId);
}
