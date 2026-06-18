import type { SystemInfoReport } from '$lib/client/types.gen';

export function memPct(sys: SystemInfoReport | null | undefined): number {
  if (!sys?.mem_total_mb || sys?.mem_used_mb == null) return 0;
  return Math.min(100, (sys.mem_used_mb / sys.mem_total_mb) * 100);
}

export function loadPct(sys: SystemInfoReport | null | undefined): number | null {
  if (sys?.load_avg_1 == null || !sys?.cpu_logical) return null;
  return Math.min(100, (sys.load_avg_1 / sys.cpu_logical) * 100);
}

export function cpuPct(sys: SystemInfoReport | null | undefined): number | null {
  return sys?.cpu_usage_percent ?? null;
}
