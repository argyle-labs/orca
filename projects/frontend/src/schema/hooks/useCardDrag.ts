import { useRef, useCallback, useState } from 'react';
import { edgePath } from '../utils/layout';
import { pointerToWorld } from '../utils/coords';

interface UseCardDragOptions {
  viewportRef: { current: HTMLDivElement | null };
  cam: { current: Cam };
  edges: Edge[];
  focusNode: (node: TableNode, zoom?: number) => void;
  setSelected: (updater: string | ((prev: string | null) => string | null)) => void;
}

export function useCardDrag({ viewportRef, cam, edges, focusNode, setSelected }: UseCardDragOptions) {
  const dragState = useRef<{ node: TableNode; ox: number; oy: number; moved: boolean } | null>(null);
  const clickState = useRef({ time: 0, id: '' });
  const [, forceUpdate] = useState(0);

  const handleCardPointerDown = useCallback((ev: PointerEvent, node: TableNode) => {
    ev.stopPropagation();

    const rect = viewportRef.current!.getBoundingClientRect();
    const { x: worldX, y: worldY } = pointerToWorld(ev.clientX, ev.clientY, rect, cam.current);

    dragState.current = { node, ox: worldX - node.x, oy: worldY - node.y, moved: false };

    const element = ev.currentTarget as HTMLElement;
    element.setPointerCapture(ev.pointerId);
    element.classList.add('dragging');
  }, []);

  const handleCardPointerMove = useCallback(
    (ev: PointerEvent, node: TableNode) => {
      const drag = dragState.current;
      if (!drag || drag.node !== node) return;

      const rect = viewportRef.current!.getBoundingClientRect();
      const { x: newX, y: newY } = pointerToWorld(ev.clientX, ev.clientY, rect, cam.current);
      const adjustedX = newX - drag.ox;
      const adjustedY = newY - drag.oy;

      if (!drag.moved && (Math.abs(adjustedX - node.x) > 3 || Math.abs(adjustedY - node.y) > 3)) {
        drag.moved = true;
      }
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
    },
    [edges]
  );

  const handleCardPointerUp = useCallback(
    (ev: PointerEvent, node: TableNode) => {
      const drag = dragState.current;
      if (!drag || drag.node !== node) return;

      const element = ev.currentTarget as HTMLElement;
      element.classList.remove('dragging');
      element.releasePointerCapture(ev.pointerId);

      if (!drag.moved) {
        const now = Date.now();
        const isDoubleClick = now - clickState.current.time < 350 && clickState.current.id === node.id;

        if (isDoubleClick) {
          focusNode(node, 1);
          setSelected(node.id);
          clickState.current = { time: 0, id: '' };
        } else {
          setSelected((prev) => (prev === node.id ? null : node.id));
          clickState.current = { time: now, id: node.id };
        }
      } else {
        forceUpdate((v) => v + 1);
      }

      dragState.current = null;
    },
    [focusNode, setSelected]
  );

  return { handleCardPointerDown, handleCardPointerMove, handleCardPointerUp };
}
