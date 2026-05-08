import { useState, useMemo, useCallback } from 'react';

export function useDomainFilter(domains: Domain[]) {
  const [activeDomains, setActiveDomains] = useState(() => new Set(domains.map((d) => d.key)));

  const legendItems = useMemo(() => {
    const seen = new Set<string>();

    return domains.reduce<LegendItem[]>((acc, domain) => {
      const key = domain.group || domain.key;
      if (seen.has(key)) return acc;
      seen.add(key);

      const groupKeys = domain.group ? domains.filter((d) => d.group === domain.group).map((d) => d.key) : [domain.key];

      return [...acc, { key, label: domain.group || domain.label, color: domain.color, groupKeys }];
    }, []);
  }, [domains]);

  const toggleDomain = useCallback(
    (keys: string[]) =>
      setActiveDomains((prev) => {
        const next = new Set(prev);
        const allActive = keys.every((k) => next.has(k));
        keys.forEach((k) => (allActive ? next.delete(k) : next.add(k)));
        return next;
      }),
    []
  );

  return { activeDomains, setActiveDomains, legendItems, toggleDomain };
}
