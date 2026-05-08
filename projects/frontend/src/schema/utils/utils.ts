export const cx = (...args: (string | false | null | undefined)[]) => args.filter(Boolean).join(' ');

export function buildDomainMap(domains: Domain[]): Record<string, Domain> {
  const map: Record<string, Domain> = {};
  for (const d of domains) {
    for (const t of d.tables) {
      map[t] = d;
    }
  }
  return map;
}
