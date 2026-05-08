import { describe, it, expect } from 'vitest';
import { pointerToWorld, fitViewParams, focusNodeParams, zoomByParams, wheelZoomParams } from './coords';

describe('pointerToWorld', () => {
  it('converts screen coords to world space at zoom=1', () => {
    const cam: Cam = { x: 0, y: 0, z: 1 };
    const rect = { left: 0, top: 0 };
    const result = pointerToWorld(100, 200, rect, cam);
    expect(result.x).toBe(100);
    expect(result.y).toBe(200);
  });

  it('accounts for camera offset', () => {
    const cam: Cam = { x: 50, y: 50, z: 1 };
    const rect = { left: 0, top: 0 };
    const result = pointerToWorld(100, 100, rect, cam);
    expect(result.x).toBe(150);
    expect(result.y).toBe(150);
  });

  it('accounts for zoom', () => {
    const cam: Cam = { x: 0, y: 0, z: 2 };
    const rect = { left: 0, top: 0 };
    const result = pointerToWorld(200, 200, rect, cam);
    expect(result.x).toBe(100);
    expect(result.y).toBe(100);
  });

  it('subtracts rect offset', () => {
    const cam: Cam = { x: 0, y: 0, z: 1 };
    const rect = { left: 100, top: 50 };
    const result = pointerToWorld(150, 100, rect, cam);
    expect(result.x).toBe(50);
    expect(result.y).toBe(50);
  });
});

describe('fitViewParams', () => {
  it('fits world into viewport with 90% margin', () => {
    const { targetZoom } = fitViewParams(1000, 800, 1000, 800);
    expect(targetZoom).toBeCloseTo(0.9, 5);
  });

  it('is limited by the narrower dimension', () => {
    // viewport 500x1000, world 1000x1000 → zoom limited by width: 500/1000 * 0.9 = 0.45
    const { targetZoom } = fitViewParams(500, 1000, 1000, 1000);
    expect(targetZoom).toBeCloseTo(0.45, 5);
  });

  it('centers the world in the viewport', () => {
    const vW = 1000, vH = 800, wW = 1000, wH = 800;
    const { targetX, targetY } = fitViewParams(vW, vH, wW, wH);
    // At zoom 0.9: world visually centered → targetX = targetY = (wW * 0.9 - vW) / 2 / 0.9
    const z = 0.9;
    expect(targetX).toBeCloseTo((wW * z - vW) / 2 / z, 3);
    expect(targetY).toBeCloseTo((wH * z - vH) / 2 / z, 3);
  });
});

describe('focusNodeParams', () => {
  it('centers the node in the viewport', () => {
    const node: TableNode = { id: 'n', table: { name: 'n', columns: [] }, x: 500, y: 400, w: 200, h: 100 };
    const { targetX, targetY, targetZoom } = focusNodeParams(1000, 800, node, 1, 1);
    // node center: (500+100, 400+50) = (600, 450)
    // targetX = 600 - 1000/2/1 = 100
    // targetY = 450 - 800/2/1 = 50
    expect(targetZoom).toBe(1);
    expect(targetX).toBeCloseTo(100, 3);
    expect(targetY).toBeCloseTo(50, 3);
  });

  it('uses zoomOverride when provided', () => {
    const node: TableNode = { id: 'n', table: { name: 'n', columns: [] }, x: 0, y: 0, w: 100, h: 100 };
    const { targetZoom } = focusNodeParams(1000, 800, node, 0.5, 2);
    expect(targetZoom).toBe(2);
  });

  it('defaults to max(currentZoom, 0.8)', () => {
    const node: TableNode = { id: 'n', table: { name: 'n', columns: [] }, x: 0, y: 0, w: 100, h: 100 };
    const { targetZoom: z1 } = focusNodeParams(1000, 800, node, 0.3);
    expect(z1).toBe(0.8);
    const { targetZoom: z2 } = focusNodeParams(1000, 800, node, 1.5);
    expect(z2).toBe(1.5);
  });
});

describe('zoomByParams', () => {
  it('zooms in by factor', () => {
    const cam: Cam = { x: 0, y: 0, z: 1 };
    const { targetZoom } = zoomByParams(1000, 800, cam, 2);
    expect(targetZoom).toBe(2);
  });

  it('clamps zoom to [0.05, 5]', () => {
    const cam: Cam = { x: 0, y: 0, z: 4.9 };
    const { targetZoom } = zoomByParams(1000, 800, cam, 10);
    expect(targetZoom).toBe(5);
  });
});

describe('wheelZoomParams', () => {
  it('zooms in when scrolling up (negative deltaY)', () => {
    const cam: Cam = { x: 0, y: 0, z: 1 };
    const rect = { left: 0, top: 0 };
    const { z } = wheelZoomParams(0, 0, rect, cam, -100, false);
    expect(z).toBeGreaterThan(1);
  });

  it('zooms out when scrolling down (positive deltaY)', () => {
    const cam: Cam = { x: 0, y: 0, z: 1 };
    const rect = { left: 0, top: 0 };
    const { z } = wheelZoomParams(0, 0, rect, cam, 100, false);
    expect(z).toBeLessThan(1);
  });

  it('zooms faster with ctrlKey (pinch gesture)', () => {
    const cam: Cam = { x: 0, y: 0, z: 1 };
    const rect = { left: 0, top: 0 };
    const { z: zNormal } = wheelZoomParams(0, 0, rect, cam, -100, false);
    const { z: zPinch } = wheelZoomParams(0, 0, rect, cam, -100, true);
    expect(zPinch).toBeGreaterThan(zNormal);
  });

  it('clamps zoom to min (0.05)', () => {
    const cam: Cam = { x: 0, y: 0, z: 0.05 };
    const rect = { left: 0, top: 0 };
    const { z } = wheelZoomParams(0, 0, rect, cam, 100000, false);
    expect(z).toBe(0.05);
  });

  it('clamps zoom to max (5)', () => {
    const cam: Cam = { x: 0, y: 0, z: 5 };
    const rect = { left: 0, top: 0 };
    const { z } = wheelZoomParams(0, 0, rect, cam, -100000, false);
    expect(z).toBe(5);
  });
});
