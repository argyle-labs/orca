import { useState, useMemo } from 'react';

export function useSearch(tables: Table[]) {
  const [searchQuery, setSearchQuery] = useState('');

  const searchMatchSet = useMemo(() => {
    if (!searchQuery) return null;
    const query = searchQuery.toLowerCase();
    const matchingTables = tables.filter(
      (table) => table.name.toLowerCase().includes(query) || table.columns.some((col) => col.name.toLowerCase().includes(query) || col.type.toLowerCase().includes(query))
    );
    return new Set(matchingTables.map((t) => t.name));
  }, [searchQuery, tables]);

  return { searchQuery, setSearchQuery, searchMatchSet };
}
