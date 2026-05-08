<script lang="ts">
  import { edgePath } from '../utils/layout';
  import { cx } from '../utils/utils';

  interface Props {
    edges: Edge[];
    width: number;
    height: number;
    isDimmed: (id: string) => boolean;
    selectedId: string | null;
    hoveredId: string | null;
  }
  let { edges, width, height, isDimmed, selectedId, hoveredId }: Props = $props();
</script>

<svg id="edges-svg" {width} {height} style="width: {width}px; height: {height}px">
  {#each edges as edge, index (index)}
    {@const path = edgePath(edge.source, edge.target)}
    {@const dim = isDimmed(edge.source.id) || isDimmed(edge.target.id)}
    {@const highlighted = !!(selectedId && (edge.source.id === selectedId || edge.target.id === selectedId))}
    {@const hovered = !!(!selectedId && hoveredId && (edge.source.id === hoveredId || edge.target.id === hoveredId))}
    <g data-edge={index}>
      <path d={path.d} class={cx(dim && 'dim', highlighted && 'highlight', hovered && 'hover')} />
      <polygon class={cx('edge-arrow', dim && 'dim', highlighted && 'highlight', hovered && 'hover')} points={path.arrow} />
      <text class="edge-label" x={path.mx} y={path.my - 6} text-anchor="middle">{edge.col}</text>
    </g>
  {/each}
</svg>
