export function createDomainFilter(domains: () => Domain[]) {
  let activeDomains = $state<Set<string>>(new Set(domains().map(d => d.key)));

  const legendItems = $derived.by<LegendItem[]>(() => {
    const ds = domains();
    const seen = new Set<string>();
    return ds.reduce<LegendItem[]>((acc, domain) => {
      const key = domain.group || domain.key;
      if (seen.has(key)) return acc;
      seen.add(key);
      const groupKeys = domain.group
        ? ds.filter(d => d.group === domain.group).map(d => d.key)
        : [domain.key];
      return [...acc, { key, label: domain.group || domain.label, color: domain.color, groupKeys }];
    }, []);
  });

  function toggleDomain(keys: string[]) {
    const next = new Set(activeDomains);
    const allActive = keys.every(k => next.has(k));
    keys.forEach(k => (allActive ? next.delete(k) : next.add(k)));
    activeDomains = next;
  }

  return {
    get activeDomains() {
      return activeDomains;
    },
    set activeDomains(v: Set<string>) {
      activeDomains = v;
    },
    get legendItems() {
      return legendItems;
    },
    toggleDomain,
  };
}
