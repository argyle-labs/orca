import React from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './css/schemaVisualizer.css';

type ApiColumn = { name: string; type: string; key?: string; nullable?: boolean; fk_target?: string | null };
type ApiTab = {
  title?: string;
  tables?: Array<{ name: string } | string>;
  columns?: Record<string, ApiColumn[]>;
  foreignKeys?: Array<{ table: string; column: string; refTable: string }>;
  domains?: Domain[];
  drift?: DriftReport;
};

function normalizeTab(tab: ApiTab): TabData {
  const colMap = tab.columns ?? {};
  const seen = new Set<string>();
  const tables: Table[] = [];

  for (const t of tab.tables ?? []) {
    const name = typeof t === 'string' ? t : t.name;
    if (seen.has(name)) continue;
    seen.add(name);
    const columns: Column[] = (colMap[name] ?? []).map((c) => ({
      name: c.name,
      type: c.type,
      pk: c.key === 'PRI',
      fk: !!c.fk_target,
      fkTarget: c.fk_target ?? undefined,
    }));
    tables.push({ name, columns });
  }

  const seenFk = new Set<string>();
  const fks: FK[] = [];
  for (const fk of tab.foreignKeys ?? []) {
    const key = `${fk.table}.${fk.column}->${fk.refTable}`;
    if (seenFk.has(key)) continue;
    seenFk.add(key);
    fks.push({ from: fk.table, fromCol: fk.column, to: fk.refTable });
  }

  return {
    title: tab.title ?? '',
    tables,
    fks,
    domains: (tab.domains as Domain[]) ?? [],
    drift: tab.drift,
  };
}

export function mountSchemaApp(container: HTMLElement, schema: { tabs: ApiTab[] }, initialTabName?: string) {
  const tabs = (schema.tabs ?? []).map(normalizeTab).filter((t) => t.tables.length > 0);
  if (tabs.length === 0) {
    container.innerHTML = '<div style="display:flex;align-items:center;justify-content:center;height:100%;color:var(--color-text-dim)">No schema data. Configure a database in System → Schema.</div>';
    return { unmount: () => { container.innerHTML = ''; } };
  }

  const data: SchemaData = { tabs, showTabs: tabs.length > 1 };
  const root = createRoot(container);
  root.render(React.createElement(App, { data, initialTabName }));
  return { unmount: () => root.unmount() };
}
