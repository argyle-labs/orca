import { edgePath } from '../utils/layout';
import { cx } from '../utils/utils';

export function EdgesSvg({
  edges,
  width,
  height,
  isDimmed,
  selectedId,
  hoveredId
}: {
  edges: Edge[];
  width: number;
  height: number;
  isDimmed: (id: string) => boolean;
  selectedId: string | null;
  hoveredId: string | null;
}) {
  return (
    <svg id="edges-svg" width={width} height={height} style={{ width: `${width}px`, height: `${height}px` }}>
      {edges.map((edge, index) => {
        const path = edgePath(edge.source, edge.target);
        const dim = isDimmed(edge.source.id) || isDimmed(edge.target.id);
        const highlighted = selectedId && (edge.source.id === selectedId || edge.target.id === selectedId);
        const hovered = !selectedId && hoveredId && (edge.source.id === hoveredId || edge.target.id === hoveredId);

        return (
          <g key={index} data-edge={index}>
            <path d={path.d} className={cx(dim && 'dim', highlighted && 'highlight', hovered && 'hover')} />
            <polygon className={cx('edge-arrow', dim && 'dim', highlighted && 'highlight', hovered && 'hover')} points={path.arrow} />
            <text className="edge-label" x={path.mx} y={path.my - 6} textAnchor="middle">
              {edge.col}
            </text>
          </g>
        );
      })}
    </svg>
  );
}
