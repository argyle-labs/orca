<script lang="ts" module>
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

  export function normalizeSchema(schema: { tabs: ApiTab[] }): SchemaData {
    const tabs = (schema.tabs ?? []).map(normalizeTab).filter((t) => t.tables.length > 0);
    return { tabs, showTabs: tabs.length > 1 };
  }
</script>

<script lang="ts">
  import { onMount } from 'svelte';
  import TabBar from './components/TabBar.svelte';
  import SchemaView from './components/SchemaView.svelte';
  import { createTheme } from './hooks/useTheme.svelte';

  interface Props {
    data: SchemaData;
    initialTabName?: string;
  }
  let { data, initialTabName }: Props = $props();

  const theme = createTheme();

  // One-shot snapshot — initial selection only.
  // svelte-ignore state_referenced_locally
  const initialTabNameSnap = initialTabName;
  // svelte-ignore state_referenced_locally
  const initialDataSnap = data;
  const initialIndex = initialTabNameSnap
    ? Math.max(0, initialDataSnap.tabs.findIndex((t) => t.title.toLowerCase() === initialTabNameSnap.toLowerCase()))
    : 0;

  let activeTab = $state(initialIndex);

  function selectTab(index: number) {
    activeTab = index;
    const name = data.tabs[index]?.title?.toLowerCase();
    if (name && typeof window !== 'undefined') {
      window.history.replaceState({}, '', `/schema/${name}`);
    }
  }

  onMount(() => theme.startObserver());
</script>

<div class={`schema-page${data.showTabs ? ' has-tabs' : ''}`}>
  {#if data.showTabs}
    <TabBar tabs={data.tabs} activeIndex={activeTab} onselect={selectTab} />
  {/if}
  {#key activeTab}
    <SchemaView
      data={data.tabs[activeTab]}
      palette={theme.palette}
      mode={theme.mode}
      onpalettechange={theme.setPalette}
      ontogglemode={theme.toggleMode}
    />
  {/key}
</div>
