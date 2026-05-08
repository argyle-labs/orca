import { MAX_VISIBLE_COLS } from '../utils/constants';
import { cx } from '../utils/utils';

export function TableCard({
  node,
  isDimmed,
  isSelected,
  isConnected,
  isSearchMatch,
  onHover,
  onLeave,
  onPointerDown,
  onPointerMove,
  onPointerUp
}: {
  node: TableNode;
  isDimmed: boolean;
  isSelected: boolean;
  isConnected: boolean;
  isSearchMatch: boolean;
  onHover: () => void;
  onLeave: () => void;
  onPointerDown: (e: PointerEvent) => void;
  onPointerMove: (e: PointerEvent) => void;
  onPointerUp: (e: PointerEvent) => void;
}) {
  const domainColor = node.domain?.color ?? '#556';

  return (
    <div
      className={cx('table-card', isDimmed && 'dim', isSelected && 'selected', isConnected && 'connected', isSearchMatch && 'search-match')}
      data-table={node.id}
      style={{ left: node.x, top: node.y }}
      onMouseEnter={onHover}
      onMouseLeave={onLeave}
      onPointerDown={(e) => onPointerDown(e as unknown as PointerEvent)}
      onPointerMove={(e) => onPointerMove(e as unknown as PointerEvent)}
      onPointerUp={(e) => onPointerUp(e as unknown as PointerEvent)}
    >
      <div className="table-header" style={{ background: domainColor }}>
        <span className="name">{node.id}</span>
        <span className="count">{node.table.columns.length} cols</span>
      </div>
      <div className="table-cols">
        {node.table.columns.slice(0, MAX_VISIBLE_COLS).map((col) => (
          <div key={col.name} className="table-col">
            <span className={cx('col-badge', col.pk ? 'pk' : col.fk ? 'fk' : 'none')}>{col.pk ? 'PK' : col.fk ? 'FK' : '--'}</span>
            <span className={cx('col-name', col.pk && 'is-pk', col.fk && 'is-fk')}>{col.name}</span>
            <span className="col-type">{col.type}</span>
          </div>
        ))}
      </div>
      {node.table.columns.length > MAX_VISIBLE_COLS && <div className="table-more">+ {node.table.columns.length - MAX_VISIBLE_COLS} more</div>}
    </div>
  );
}
