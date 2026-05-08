<script lang="ts">
  import Toolbar from './Toolbar.svelte';
  import TableGrid from './TableGrid.svelte';
  import BrowseDetailPanel from './BrowseDetailPanel.svelte';
  import CanvasPanel from './CanvasPanel.svelte';
  import { createSearch } from '../hooks/useSearch.svelte';
  import { createDomainFilter } from '../hooks/useDomainFilter.svelte';
  import type { Palette, Mode } from '../hooks/useTheme.svelte';

  type ViewMode = 'browse' | 'canvas';

  interface Props {
    data: TabData;
    palette: Palette;
    mode: Mode;
    onpalettechange: (p: Palette) => void;
    ontogglemode: () => void;
  }
  let { data, palette, mode, onpalettechange, ontogglemode }: Props = $props();

  const tables = $derived(data.tables);
  const fks = $derived(data.fks);
  const domains = $derived(data.domains);
  const title = $derived(data.title);
  const drift = $derived(data.drift);

  let viewMode = $state<ViewMode>('browse');
  let selected = $state<string | null>(null);

  const search = createSearch(() => tables);
  const filter = createDomainFilter(() => domains);

  function handleSelect(id: string) {
    selected = selected === id ? null : id;
  }

  function handleGoTo(id: string) {
    selected = id;
    if (viewMode !== 'canvas') viewMode = 'browse';
  }

  function setSelectedFromCanvas(value: string | ((prev: string | null) => string | null)) {
    if (typeof value === 'function') selected = value(selected);
    else selected = value === '' ? null : value;
  }
</script>

<Toolbar
  {title}
  tableCount={tables.length}
  fkCount={fks.length}
  searchQuery={search.searchQuery}
  onsearchchange={(q) => (search.searchQuery = q)}
  legendItems={filter.legendItems}
  activeDomains={filter.activeDomains}
  ontoggledomain={filter.toggleDomain}
  {palette}
  themeMode={mode}
  {onpalettechange}
  {ontogglemode}
  {viewMode}
  onviewmodechange={(m) => (viewMode = m)}
/>

{#if viewMode === 'browse'}
  <div class="schema-browse">
    <TableGrid
      {tables}
      {fks}
      {domains}
      {selected}
      onselect={handleSelect}
      searchQuery={search.searchQuery}
      activeDomains={filter.activeDomains}
    />
    <BrowseDetailPanel
      selectedId={selected}
      {tables}
      {fks}
      {domains}
      onclose={() => (selected = null)}
      ongoto={handleGoTo}
    />
  </div>
{:else if viewMode === 'canvas'}
  {#key data}
    <CanvasPanel
      {data}
      searchQuery={search.searchQuery}
      searchMatchSet={search.searchMatchSet}
      activeDomains={filter.activeDomains}
      {selected}
      onselect={setSelectedFromCanvas}
      ongoto={handleGoTo}
      {drift}
    />
  {/key}
{/if}
