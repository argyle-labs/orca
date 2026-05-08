import { cx } from '../utils/utils';
import { getOutgoingEdges, getIncomingEdges } from '../utils/graph';

export function DetailPanel({
  selectedId,
  selectedNode,
  edges,
  nodeMap,
  onClose,
  onGoTo
}: {
  selectedId: string | null;
  selectedNode: TableNode | null;
  edges: Edge[];
  nodeMap: Record<string, TableNode>;
  onClose: () => void;
  onGoTo: (id: string) => void;
}) {
  const domainInfo = selectedNode?.domain ?? { color: '#556', label: 'Other' };

  const outgoingEdges = selectedId ? getOutgoingEdges(edges, selectedId) : [];
  const incomingEdges = selectedId ? getIncomingEdges(edges, selectedId) : [];

  return (
    <div id="detail" className={selectedId ? 'open' : ''}>
      <div id="detail-header">
        <div>
          <h2>{selectedId ?? ''}</h2>
          {selectedNode && (
            <span className="domain-badge" style={{ background: domainInfo.color + '22', color: domainInfo.color }}>
              {domainInfo.label}
            </span>
          )}
        </div>
        <button id="detail-close" onClick={onClose}>
          &times;
        </button>
      </div>

      {selectedNode && (
        <>
          <div id="detail-columns">
            {selectedNode.table.columns.map((col) => {
              const isClickable = col.fk && col.fkTarget && nodeMap[col.fkTarget];

              return (
                <div key={col.name} className={cx('detail-col', col.fk && 'fk-row')} onClick={isClickable ? () => onGoTo(col.fkTarget!) : undefined}>
                  <span className={cx('col-badge-d', col.pk && 'pk', col.fk && 'fk')} style={!col.pk && !col.fk ? { opacity: 0 } : {}}>
                    {col.pk ? 'PK' : col.fk ? 'FK' : '--'}
                  </span>
                  <span className="col-name-d">{col.name}</span>
                  <span className="col-type-d">{col.type}</span>
                  {col.fk && col.fkTarget && <span className="col-arrow">&rarr; {col.fkTarget}</span>}
                </div>
              );
            })}
          </div>

          <div id="detail-relations">
            <h3>Relationships</h3>
            <div id="detail-rels-list">
              {outgoingEdges.map((edge) => (
                <div key={edge.col + edge.target.id} className="relation-item" onClick={() => onGoTo(edge.target.id)}>
                  <span className="relation-dir">&rarr;</span>
                  <span className="relation-table">{edge.target.id}</span>
                  <span className="relation-via">via {edge.col}</span>
                </div>
              ))}
              {incomingEdges.map((edge) => (
                <div key={edge.col + edge.source.id} className="relation-item" onClick={() => onGoTo(edge.source.id)}>
                  <span className="relation-dir">&larr;</span>
                  <span className="relation-table">{edge.source.id}</span>
                  <span className="relation-via">via {edge.col}</span>
                </div>
              ))}
            </div>
          </div>
        </>
      )}
    </div>
  );
}
