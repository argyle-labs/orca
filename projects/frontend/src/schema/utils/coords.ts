import { clampZoom } from './animation';

/** Convert a pointer event's client position to world-space coordinates. */
export function pointerToWorld(
  clientX: number,
  clientY: number,
  rect: { left: number; top: number },
  cam: Cam,
): { x: number; y: number } {
  return {
    x: (clientX - rect.left) / cam.z + cam.x,
    y: (clientY - rect.top) / cam.z + cam.y,
  };
}

/** Compute camera params to fit the entire world into the viewport. */
export function fitViewParams(
  viewWidth: number,
  viewHeight: number,
  wW: number,
  wH: number,
): { targetZoom: number; targetX: number; targetY: number } {
  const targetZoom = Math.min(viewWidth / wW, viewHeight / wH) * 0.9;
  return {
    targetZoom,
    targetX: (wW * targetZoom - viewWidth) / 2 / targetZoom,
    targetY: (wH * targetZoom - viewHeight) / 2 / targetZoom,
  };
}

/** Compute camera params to center a node in the viewport. */
export function focusNodeParams(
  viewWidth: number,
  viewHeight: number,
  node: TableNode,
  currentZoom: number,
  zoomOverride?: number,
): { targetX: number; targetY: number; targetZoom: number } {
  const targetZoom = zoomOverride ?? Math.max(currentZoom, 0.8);
  return {
    targetZoom,
    targetX: node.x + node.w / 2 - viewWidth / 2 / targetZoom,
    targetY: node.y + node.h / 2 - viewHeight / 2 / targetZoom,
  };
}

/** Compute new camera state after zooming by a factor around the viewport center. */
export function zoomByParams(
  viewWidth: number,
  viewHeight: number,
  cam: Cam,
  factor: number,
): { targetX: number; targetY: number; targetZoom: number } {
  const centerX = viewWidth / 2 / cam.z + cam.x;
  const centerY = viewHeight / 2 / cam.z + cam.y;
  const targetZoom = clampZoom(cam.z * factor);
  return {
    targetZoom,
    targetX: centerX - viewWidth / 2 / targetZoom,
    targetY: centerY - viewHeight / 2 / targetZoom,
  };
}

/** Compute new camera state after a wheel scroll event. Returns updated x/y/z. */
export function wheelZoomParams(
  clientX: number,
  clientY: number,
  rect: { left: number; top: number; width?: number; height?: number },
  cam: Cam,
  deltaY: number,
  ctrlKey: boolean,
): { x: number; y: number; z: number } {
  const delta = ctrlKey ? -deltaY * 0.01 : -deltaY * 0.002;
  const newZ = clampZoom(cam.z * Math.pow(2, delta));
  const mouseX = clientX - rect.left;
  const mouseY = clientY - rect.top;
  return {
    x: mouseX / cam.z + cam.x - mouseX / newZ,
    y: mouseY / cam.z + cam.y - mouseY / newZ,
    z: newZ,
  };
}
