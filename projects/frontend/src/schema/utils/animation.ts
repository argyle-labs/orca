/** Quadratic ease-in-out. progress should be in [0, 1]. */
export function easeInOutQuad(progress: number): number {
  return progress < 0.5 ? 2 * progress * progress : -1 + (4 - 2 * progress) * progress;
}

export function clampZoom(z: number, min = 0.05, max = 5): number {
  return Math.max(min, Math.min(max, z));
}
