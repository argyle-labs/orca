import { useMemo, useState } from 'react';
import { cx } from '../utils/utils';

interface TableGridProps {
  tables: Table[];
  fks: FK[];
  domains: Domain[];
  selected: string | null;
  onSelect: (id: string) => void;
  searchQuery: string;
  activeDomains: Set<string>;
}

type HierarchyEntry   = { domain: Domain; tables: Table[] };
type SubgroupEntry    = { key: string; label: string; entries: HierarchyEntry[] };
type GroupEntry       = { key: string; label: string; color: string; subgroups: SubgroupEntry[]; total: number };

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

export function TableGrid({ tables, fks, domains, selected, onSelect, searchQuery, activeDomains }: TableGridProps) {
  const domainOf = useMemo(() => {
    const m: Record<string, Domain> = {};
    for (const d of domains) for (const t of d.tables) m[t] = d;
    return m;
  }, [domains]);

  const fkOutCount = useMemo(() => {
    const c: Record<string, number> = {};
    for (const fk of fks) c[fk.from] = (c[fk.from] ?? 0) + 1;
    return c;
  }, [fks]);

  const fkInCount = useMemo(() => {
    const c: Record<string, number> = {};
    for (const fk of fks) c[fk.to] = (c[fk.to] ?? 0) + 1;
    return c;
  }, [fks]);

  const filteredTables = useMemo(() => {
    const q = searchQuery.toLowerCase();
    return tables.filter((t) => {
      const domain = domainOf[t.name];
      if (domain && !activeDomains.has(domain.key)) return false;
      if (!q) return true;
      return t.name.toLowerCase().includes(q) || t.columns.some((c) => c.name.toLowerCase().includes(q));
    });
  }, [tables, searchQuery, activeDomains, domainOf]);

  const hierarchy = useMemo(
    () => buildHierarchy(filteredTables, domains, domainOf),
    [filteredTables, domains, domainOf],
  );

  if (filteredTables.length === 0) {
    return <div className="tg-empty">No tables match your search or filter.</div>;
  }

  return (
    <div className="tg-scroll">
      {hierarchy.map((group) => (
        <GroupSection key={group.key} group={group} selected={selected} onSelect={onSelect}
          fkOutCount={fkOutCount} fkInCount={fkInCount} />
      ))}
    </div>
  );
}

function GroupSection({ group, selected, onSelect, fkOutCount, fkInCount }: {
  group: GroupEntry;
  selected: string | null;
  onSelect: (id: string) => void;
  fkOutCount: Record<string, number>;
  fkInCount: Record<string, number>;
}) {
  const [collapsed, setCollapsed] = useState(false);

  return (
    <section className="tg-group">
      <button className="tg-group-header" onClick={() => setCollapsed((c) => !c)}>
        <span className="tg-group-label">{group.label}</span>
        <span className="tg-group-count">{group.total}</span>
        <span className="tg-group-chevron">{collapsed ? '▸' : '▾'}</span>
      </button>

      {!collapsed && (
        <div className="tg-group-body">
          {group.subgroups.map((sg) => (
            <div key={sg.key} className={sg.label ? 'tg-subgroup' : undefined}>
              {sg.label && <div className="tg-subgroup-header">{sg.label}</div>}
              {sg.entries.map(({ domain, tables }) => (
                <DomainSection key={domain.key} domain={domain} tables={tables} selected={selected} onSelect={onSelect}
                  fkOutCount={fkOutCount} fkInCount={fkInCount} />
              ))}
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

function DomainSection({ domain, tables, selected, onSelect, fkOutCount, fkInCount }: {
  domain: Domain;
  tables: Table[];
  selected: string | null;
  onSelect: (id: string) => void;
  fkOutCount: Record<string, number>;
  fkInCount: Record<string, number>;
}) {
  const [collapsed, setCollapsed] = useState(false);

  return (
    <section className="tg-section">
      <button className="tg-section-header" onClick={() => setCollapsed((c) => !c)} style={{ borderColor: domain.color }}>
        <span className="tg-section-dot" style={{ background: domain.color }} />
        <span className="tg-section-label">{domain.label}</span>
        <span className="tg-section-count">{tables.length}</span>
        <span className="tg-section-chevron">{collapsed ? '▸' : '▾'}</span>
      </button>

      {!collapsed && (
        <div className="tg-grid">
          {tables.map((table) => (
            <TableCard
              key={table.name}
              table={table}
              domain={domain}
              isSelected={selected === table.name}
              fkOut={fkOutCount[table.name] ?? 0}
              fkIn={fkInCount[table.name] ?? 0}
              onSelect={onSelect}
            />
          ))}
        </div>
      )}
    </section>
  );
}

function TableCard({ table, domain, isSelected, fkOut, fkIn, onSelect }: {
  table: Table;
  domain: Domain;
  isSelected: boolean;
  fkOut: number;
  fkIn: number;
  onSelect: (id: string) => void;
}) {
  const pkCols = table.columns.filter((c) => c.pk);
  const fkCols = table.columns.filter((c) => c.fk);

  return (
    <button
      className={cx('tg-card', isSelected && 'tg-card-selected')}
      style={{ '--domain-color': domain.color } as React.CSSProperties}
      onClick={() => onSelect(table.name)}
    >
      <div className="tg-card-header">
        <span className="tg-card-name">{table.name}</span>
        <span className="tg-card-cols">{table.columns.length} cols</span>
      </div>
      <div className="tg-card-meta">
        {pkCols.length > 0 && <span className="tg-badge tg-badge-pk">PK·{pkCols.length}</span>}
        {fkCols.length > 0 && <span className="tg-badge tg-badge-fk">FK·{fkCols.length}</span>}
        {fkIn > 0 && <span className="tg-badge tg-badge-ref">←{fkIn}</span>}
      </div>
    </button>
  );
}
