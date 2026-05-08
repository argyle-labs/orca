import { edgePath } from '../utils/layout';
import { pointerToWorld } from '../utils/coords';

interface CreateCardDragOptions {
  getViewport: () => HTMLDivElement | null | undefined;
  cam: { current: Cam };
  edges: Edge[];
  focusNode: (node: TableNode, zoom?: number) => void;
  setSelected: (updater: string | ((prev: string | null) => string | null)) => void;
  bumpVersion?: () => void;
}

export function createCardDrag({ getViewport, cam, edges, focusNode, setSelected, bumpVersion }: CreateCardDragOptions) {
  const dragState: { current: { node: TableNode; ox: number; oy: number; moved: boolean } | null } = { current: null };
  const clickState = { time: 0, id: '' };

  function handleCardPointerDown(ev: PointerEvent, node: TableNode) {
    ev.stopPropagation();
    const vp = getViewport();
    if (!vp) return;
    const rect = vp.getBoundingClientRect();
    const { x: worldX, y: worldY } = pointerToWorld(ev.clientX, ev.clientY, rect, cam.current);
    dragState.current = { node, ox: worldX - node.x, oy: worldY - node.y, moved: false };
    const element = ev.currentTarget as HTMLElement;
    element.setPointerCapture(ev.pointerId);
    element.classList.add('dragging');
  }

  function handleCardPointerMove(ev: PointerEvent, node: TableNode) {
    const drag = dragState.current;
    if (!drag || drag.node !== node) return;
    const vp = getViewport();
    if (!vp) return;
    const rect = vp.getBoundingClientRect();
    const { x: newX, y: newY } = pointerToWorld(ev.clientX, ev.clientY, rect, cam.current);
    const adjustedX = newX - drag.ox;
    const adjustedY = newY - drag.oy;
    if (!drag.moved && (Math.abs(adjustedX - node.x) > 3 || Math.abs(adjustedY - node.y) > 3)) drag.moved = true;
    if (!drag.moved) return;

    node.x = adjustedX;
    node.y = adjustedY;
    const element = ev.currentTarget as HTMLElement;
    element.style.left = `${node.x}px`;
    element.style.top = `${node.y}px`;

    edges.forEach((edge, index) => {
      if (edge.source.id !== node.id && edge.target.id !== node.id) return;
      const path = edgePath(edge.source, edge.target);
      const edgeGroup = document.querySelector(`[data-edge="${index}"]`);
      if (!edgeGroup) return;
      edgeGroup.querySelector('path')!.setAttribute('d', path.d);
      edgeGroup.querySelector('polygon')!.setAttribute('points', path.arrow);
      const label = edgeGroup.querySelector('text');
      if (label) {
        label.setAttribute('x', String(path.mx));
        label.setAttribute('y', String(path.my - 6));
      }
    });
  }

  function handleCardPointerUp(ev: PointerEvent, node: TableNode) {
    const drag = dragState.current;
    if (!drag || drag.node !== node) return;
    const element = ev.currentTarget as HTMLElement;
    element.classList.remove('dragging');
    element.releasePointerCapture(ev.pointerId);

    if (!drag.moved) {
      const now = Date.now();
      const isDoubleClick = now - clickState.time < 350 && clickState.id === node.id;
      if (isDoubleClick) {
        focusNode(node, 1);
        setSelected(node.id);
        clickState.time = 0;
        clickState.id = '';
      } else {
        setSelected((prev) => (prev === node.id ? null : node.id));
        clickState.time = now;
        clickState.id = node.id;
      }
    } else {
      bumpVersion?.();
    }
    dragState.current = null;
  }

  return { handleCardPointerDown, handleCardPointerMove, handleCardPointerUp };
}
