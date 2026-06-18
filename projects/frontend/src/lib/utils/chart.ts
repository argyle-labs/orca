// Sparkline / histogram path math. Splits a value series into contiguous
// segments separated by NaN/Infinity gaps, returning one SVG line path
// and matching closed-area path per segment.

export interface ChartSegment {
  line: string;
  area: string;
}

export function chartSegments(vals: number[], W: number, H: number, vmax: number): ChartSegment[] {
  const out: ChartSegment[] = [];
  if (!vals.length) return out;
  const n = vals.length;
  let line = '';
  let area = '';
  let segStartX: number | null = null;
  let segLastX: number | null = null;
  const flush = () => {
    if (line) {
      out.push({
        line: line.trim(),
        area: `${area} L ${segLastX!.toFixed(1)} ${H} L ${segStartX!.toFixed(1)} ${H} Z`.trim(),
      });
    }
    line = '';
    area = '';
    segStartX = null;
    segLastX = null;
  };
  for (let i = 0; i < n; i++) {
    const v = vals[i];
    const x = (i / Math.max(1, n - 1)) * W;
    if (!Number.isFinite(v)) {
      flush();
      continue;
    }
    const y = H - (Math.min(Math.max(v, 0), vmax) / vmax) * H;
    if (line === '') {
      line = `M ${x.toFixed(1)} ${y.toFixed(1)} `;
      area = `M ${x.toFixed(1)} ${y.toFixed(1)} `;
      segStartX = x;
    } else {
      line += `L ${x.toFixed(1)} ${y.toFixed(1)} `;
      area += `L ${x.toFixed(1)} ${y.toFixed(1)} `;
    }
    segLastX = x;
  }
  flush();
  return out;
}
