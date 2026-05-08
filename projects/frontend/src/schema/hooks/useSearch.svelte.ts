export function createSearch(tables: () => Table[]) {
  let searchQuery = $state('');

  const searchMatchSet = $derived.by<Set<string> | null>(() => {
    if (!searchQuery) return null;
    const query = searchQuery.toLowerCase();
    const matchingTables = tables().filter(
      (table) =>
        table.name.toLowerCase().includes(query) ||
        table.columns.some((col) =>
          col.name.toLowerCase().includes(query) || col.type.toLowerCase().includes(query)
        )
    );
    return new Set(matchingTables.map((t) => t.name));
  });

  return {
    get searchQuery() { return searchQuery; },
    set searchQuery(v: string) { searchQuery = v; },
    get searchMatchSet() { return searchMatchSet; },
  };
}
