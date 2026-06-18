export function inferChannel(
  version: string | null | undefined,
  fallback: string | null | undefined,
): string {
  const v = version ?? '';
  if (/-dev/i.test(v)) return 'dev';
  if (/-rc/i.test(v)) return 'rc';
  if (v) return 'stable';
  return fallback ?? 'stable';
}

export function instChannel(
  i: { version: string | null; pinnedTo?: string | null; channel?: string | null } | null,
): string {
  if (!i) return 'stable';
  return inferChannel(i.pinnedTo ?? i.version, i.channel);
}
