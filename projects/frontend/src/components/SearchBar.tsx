import { useEffect, useRef, useState } from 'react';
import { useNavigate } from '@tanstack/react-router';

interface SearchResult {
  root: string;
  path: string;
  matches: string[];
}

export function SearchBar() {
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<SearchResult[]>([]);
  const navigate = useNavigate();
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (timerRef.current) clearTimeout(timerRef.current);
    if (!query.trim()) { setResults([]); return; }
    timerRef.current = setTimeout(() => {
      fetch(`/api/search?q=${encodeURIComponent(query)}&root=all`)
        .then((r) => r.json())
        .then((data: SearchResult[]) => setResults(data))
        .catch(() => setResults([]));
    }, 300);
    return () => { if (timerRef.current) clearTimeout(timerRef.current); };
  }, [query]);

  function handleSelect(result: SearchResult) {
    navigate({ to: `/${result.root}/${result.path}` });
    setQuery('');
    setResults([]);
  }

  function handleCtx7() {
    navigate({ to: '/ctx7', search: { q: query } as any });
    setQuery('');
    setResults([]);
  }

  function handleKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === 'Escape') { setQuery(''); setResults([]); }
  }

  const showDropdown = query.trim().length > 0 && (results.length > 0 || query.trim().length >= 2);

  return (
    <div className="search-wrap">
      <input
        className="search-input"
        type="text"
        placeholder="Search docs…"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={handleKeyDown}
      />
      {showDropdown && (
        <div className="search-results">
          {results.map((r) => (
            <div
              key={`${r.root}/${r.path}`}
              className="search-result"
              onClick={() => handleSelect(r)}
            >
              <div className="search-result-path">{r.root}/{r.path}</div>
              {r.matches[0] && (
                <div className="search-result-match">{r.matches[0]}</div>
              )}
            </div>
          ))}
          {query.trim().length >= 2 && (
            <div className="search-result search-result-ctx7" onClick={handleCtx7}>
              <div className="search-result-path">Context7 docs →</div>
              <div className="search-result-match">Search "{query}" in library docs via MCP</div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
