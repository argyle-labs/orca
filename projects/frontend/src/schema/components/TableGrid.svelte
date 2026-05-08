<script lang="ts" module>
  type HierarchyEntry = { domain: Domain; tables: Table[] };
  type SubgroupEntry = { key: string; label: string; entries: HierarchyEntry[] };
  type GroupEntry = { key: string; label: string; color: string; subgroups: SubgroupEntry[]; total: number };

  function buildHierarchy(
    filteredTables: Table[],
    domains: Domain[],
    domainOf: Record<string, Domain>,
  ): GroupEntry[] {
    const groupOrder: string[] = [];
    const groupSeen = new Set<string>();
    for (const d of domains) {
      const g = d.group ?? d.key;
      if (!groupSeen.has(g)) { groupSeen.add(g); groupOrder.push(g); }
    }

    const result: GroupEntry[] = [];

    for (const gKey of groupOrder) {
      const gDomains = domains.filter((d) => (d.group ?? d.key) === gKey);

      const sgOrder: string[] = [];
      const sgSeen = new Set<string>();
      for (const d of gDomains) {
        const sg = d.subgroup ?? '__';
        if (!sgSeen.has(sg)) { sgSeen.add(sg); sgOrder.push(sg); }
      }

      const subgroups: SubgroupEntry[] = [];
      for (const sgKey of sgOrder) {
        const sgDomains = gDomains.filter((d) => (d.subgroup ?? '__') === sgKey);
        const entries: HierarchyEntry[] = [];
        for (const d of sgDomains) {
          const rows = filteredTables.filter((t) => domainOf[t.name]?.key === d.key);
          if (rows.length > 0) entries.push({ domain: d, tables: rows });
        }
        if (entries.length > 0) {
          subgroups.push({ key: sgKey, label: sgKey === '__' ? '' : sgKey, entries });
        }
      }

      const total = subgroups.reduce(
        (s, sg) => s + sg.entries.reduce((s2, e) => s2 + e.tables.length, 0), 0,
      );
      if (total > 0) {
        result.push({ key: gKey, label: gKey, color: gDomains[0].color, subgroups, total });
      }
    }

    const uncat = filteredTables.filter((t) => !domainOf[t.name]);
    if (uncat.length > 0) {
      const d: Domain = { key: '__other', label: 'Other', color: '#556', tables: [] };
      result.push({
        key: '__other', label: 'Other', color: '#556',
        subgroups: [{ key: '__', label: '', entries: [{ domain: d, tables: uncat }] }],
        total: uncat.length,
      });
    }

    return result;
  }

  export type { GroupEntry };
</script>

<script lang="ts">
  import GroupSection from './_TgGroupSection.svelte';

  interface Props {
    tables: Table[];
    fks: FK[];
    domains: Domain[];
    selected: string | null;
    onselect: (id: string) => void;
    searchQuery: string;
    activeDomains: Set<string>;
  }
  let { tables, fks, domains, selected, onselect, searchQuery, activeDomains }: Props = $props();

  const domainOf = $derived.by(() => {
    const m: Record<string, Domain> = {};
    for (const d of domains) for (const t of d.tables) m[t] = d;
    return m;
  });

  const fkOutCount = $derived.by(() => {
    const c: Record<string, number> = {};
    for (const fk of fks) c[fk.from] = (c[fk.from] ?? 0) + 1;
    return c;
  });

  const fkInCount = $derived.by(() => {
    const c: Record<string, number> = {};
    for (const fk of fks) c[fk.to] = (c[fk.to] ?? 0) + 1;
    return c;
  });

  const filteredTables = $derived.by(() => {
    const q = searchQuery.toLowerCase();
    return tables.filter((t) => {
      const domain = domainOf[t.name];
      if (domain && !activeDomains.has(domain.key)) return false;
      if (!q) return true;
      return t.name.toLowerCase().includes(q) || t.columns.some((c) => c.name.toLowerCase().includes(q));
    });
  });

  const hierarchy = $derived(buildHierarchy(filteredTables, domains, domainOf));
</script>

{#if filteredTables.length === 0}
  <div class="tg-empty">No tables match your search or filter.</div>
{:else}
  <div class="tg-scroll">
    {#each hierarchy as group (group.key)}
      <GroupSection {group} {selected} {onselect} {fkOutCount} {fkInCount} />
    {/each}
  </div>
{/if}
