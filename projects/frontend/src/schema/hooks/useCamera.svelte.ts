import { easeInOutQuad, clampZoom } from '../utils/animation';
import { fitViewParams, focusNodeParams, zoomByParams, wheelZoomParams } from '../utils/coords';

interface CreateCameraOptions {
  getViewport: () => HTMLDivElement | null | undefined;
  getWorld: () => HTMLDivElement | null | undefined;
  wW: number;
  wH: number;
  onBackgroundClick: () => void;
}

export function createCamera({
  getViewport,
  getWorld,
  wW,
  wH,
  onBackgroundClick,
}: CreateCameraOptions) {
  const cam: { current: Cam } = { current: { x: 0, y: 0, z: 1 } };

  function applyCameraTransform() {
    const c = cam.current;
    const w = getWorld();
    if (w) w.style.transform = `scale(${c.z}) translate(${-c.x}px,${-c.y}px)`;
  }

  function animateCamera(targetX: number, targetY: number, targetZoom: number, duration = 300) {
    const { x: startX, y: startY, z: startZoom } = cam.current;
    const startTime = performance.now();

    const step = (now: number) => {
      const progress = Math.min((now - startTime) / duration, 1);
      const ease = easeInOutQuad(progress);
      cam.current.x = startX + (targetX - startX) * ease;
      cam.current.y = startY + (targetY - startY) * ease;
      cam.current.z = startZoom + (targetZoom - startZoom) * ease;
      applyCameraTransform();
      if (progress < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }

  function fitAll(animate = true) {
    const viewport = getViewport();
    if (!viewport) return;
    const { targetZoom, targetX, targetY } = fitViewParams(
      viewport.clientWidth,
      viewport.clientHeight,
      wW,
      wH,
    );
    if (animate) {
      animateCamera(targetX, targetY, targetZoom);
    } else {
      Object.assign(cam.current, { x: targetX, y: targetY, z: targetZoom });
      applyCameraTransform();
    }
  }

  function focusNode(node: TableNode, zoom?: number) {
    const viewport = getViewport();
    if (!viewport) return;
    const { targetX, targetY, targetZoom } = focusNodeParams(
      viewport.clientWidth,
      viewport.clientHeight,
      node,
      cam.current.z,
      zoom,
    );
    animateCamera(targetX, targetY, targetZoom);
  }

  function zoomBy(factor: number) {
    const viewport = getViewport();
    if (!viewport) return;
    const { targetX, targetY, targetZoom } = zoomByParams(
      viewport.clientWidth,
      viewport.clientHeight,
      cam.current,
      factor,
    );
    animateCamera(targetX, targetY, targetZoom, 200);
  }

  function attach(): () => void {
    const viewport = getViewport();
    if (!viewport) return () => {};

    fitAll(false);

    let timeout: number;
    const handleResize = () => {
      clearTimeout(timeout);
      timeout = setTimeout(() => fitAll(true), 150) as unknown as number;
    };
    window.addEventListener('resize', handleResize);

    let isPanning = false;
    let panStart = { x: 0, y: 0 };
    let camStart = { x: 0, y: 0 };
    let hasMoved = false;

    const handlePointerDown = (ev: PointerEvent) => {
      const target = ev.target as HTMLElement;
      const world = getWorld();
      const isBackground =
        target === viewport ||
        target === world ||
        target.tagName === 'svg' ||
        target.id === 'edges-svg';
      if (isBackground) {
        isPanning = true;
        hasMoved = false;
        panStart = { x: ev.clientX, y: ev.clientY };
        camStart = { x: cam.current.x, y: cam.current.y };
        viewport.classList.add('grabbing');
        viewport.setPointerCapture(ev.pointerId);
      }
    };

    const handlePointerMove = (ev: PointerEvent) => {
      if (!isPanning) return;
      if (Math.abs(ev.clientX - panStart.x) > 3 || Math.abs(ev.clientY - panStart.y) > 3)
        hasMoved = true;
      cam.current.x = camStart.x - (ev.clientX - panStart.x) / cam.current.z;
      cam.current.y = camStart.y - (ev.clientY - panStart.y) / cam.current.z;
      applyCameraTransform();
    };

    const handlePointerUp = (ev: PointerEvent) => {
      if (!isPanning) return;
      isPanning = false;
      viewport.classList.remove('grabbing');
      viewport.releasePointerCapture(ev.pointerId);
      if (!hasMoved) onBackgroundClick();
    };

    const handleWheel = (ev: WheelEvent) => {
      ev.preventDefault();
      const rect = viewport.getBoundingClientRect();
      const next = wheelZoomParams(
        ev.clientX,
        ev.clientY,
        rect,
        cam.current,
        ev.deltaY,
        ev.ctrlKey,
      );
      Object.assign(cam.current, next);
      applyCameraTransform();
    };

    let lastPinchDist = 0;
    let lastPinchMid = { x: 0, y: 0 };

    const handleTouchStart = (ev: TouchEvent) => {
      if (ev.touches.length === 2) {
        ev.preventDefault();
        const [t0, t1] = [ev.touches[0], ev.touches[1]];
        lastPinchDist = Math.hypot(t1.clientX - t0.clientX, t1.clientY - t0.clientY);
        lastPinchMid = { x: (t0.clientX + t1.clientX) / 2, y: (t0.clientY + t1.clientY) / 2 };
      }
    };

    const handleTouchMove = (ev: TouchEvent) => {
      if (ev.touches.length !== 2) return;
      ev.preventDefault();
      const [t0, t1] = [ev.touches[0], ev.touches[1]];
      const dist = Math.hypot(t1.clientX - t0.clientX, t1.clientY - t0.clientY);
      const mid = { x: (t0.clientX + t1.clientX) / 2, y: (t0.clientY + t1.clientY) / 2 };
      const rect = viewport.getBoundingClientRect();
      const newZ = clampZoom(cam.current.z * (dist / lastPinchDist));
      const dx = mid.x - lastPinchMid.x;
      const dy = mid.y - lastPinchMid.y;
      const mx = mid.x - rect.left;
      const my = mid.y - rect.top;
      cam.current.x = mx / cam.current.z + cam.current.x - mx / newZ - dx / newZ;
      cam.current.y = my / cam.current.z + cam.current.y - my / newZ - dy / newZ;
      cam.current.z = newZ;
      applyCameraTransform();
      lastPinchDist = dist;
      lastPinchMid = mid;
    };

    viewport.addEventListener('pointerdown', handlePointerDown);
    viewport.addEventListener('pointermove', handlePointerMove);
    viewport.addEventListener('pointerup', handlePointerUp);
    viewport.addEventListener('wheel', handleWheel, { passive: false });
    viewport.addEventListener('touchstart', handleTouchStart, { passive: false });
    viewport.addEventListener('touchmove', handleTouchMove, { passive: false });

    return () => {
      window.removeEventListener('resize', handleResize);
      viewport.removeEventListener('pointerdown', handlePointerDown);
      viewport.removeEventListener('pointermove', handlePointerMove);
      viewport.removeEventListener('pointerup', handlePointerUp);
      viewport.removeEventListener('wheel', handleWheel);
      viewport.removeEventListener('touchstart', handleTouchStart);
      viewport.removeEventListener('touchmove', handleTouchMove);
    };
  }

  return { cam, fitAll, focusNode, zoomBy, attach };
}
