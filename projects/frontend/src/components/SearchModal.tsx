import { useEffect, useRef, useState } from 'react';
import { useNavigate } from '@tanstack/react-router';

interface DocResult   { kind: 'doc';   root: string; path: string; matches: string[] }
interface TableResult { kind: 'table'; table: string; domain: string; group: string; color: string }
type AnyResult = DocResult | TableResult | { kind: 'ctx7' };

interface Domain { key: string; label: string; group?: string; color: string; tables: string[] }

export function SearchModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [query, setQuery]           = useState('');
  const [docs, setDocs]             = useState<DocResult[]>([]);
  const [tables, setTables]         = useState<TableResult[]>([]);
  const [domains, setDomains]       = useState<Domain[]>([]);
  const [cursor, setCursor]         = useState(0);
  const navigate  = useNavigate();
  const inputRef  = useRef<HTMLInputElement>(null);
  const docTimer  = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Load domain index once (no DB connection needed)
  useEffect(() => {
    if (domains.length > 0) return;
    fetch('/api/schema/domains')
      .then((r) => r.ok ? r.json() : [])
      .then((d: Domain[]) => setDomains(d))
      .catch(() => {});
  }, [domains.length]);

  // Focus + reset on open
  useEffect(() => {
    if (open) {
      setQuery(''); setDocs([]); setTables([]); setCursor(0);
      setTimeout(() => inputRef.current?.focus(), 10);
    }
  }, [open]);

  // Escape to close
  useEffect(() => {
    if (!open) return;
    const h = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', h);
    return () => document.removeEventListener('keydown', h);
  }, [open, onClose]);

  // Debounced doc search
  useEffect(() => {
    if (docTimer.current) clearTimeout(docTimer.current);
    if (!query.trim()) { setDocs([]); setTables([]); setCursor(0); return; }

    // Table search is synchronous (client-side against cached domains)
    const q = query.toLowerCase();
    const matched: TableResult[] = [];
    for (const d of domains) {
      for (const t of d.tables) {
        if (t.toLowerCase().includes(q)) {
          matched.push({ kind: 'table', table: t, domain: d.label, group: d.group ?? d.label, color: d.color });
          if (matched.length >= 12) break;
        }
      }
      if (matched.length >= 12) break;
    }
    setTables(matched);

    // Async doc search
    docTimer.current = setTimeout(() => {
      fetch(`/api/search?q=${encodeURIComponent(query)}&root=all`)
        .then((r) => r.json())
        .then((data: Omit<DocResult, 'kind'>[]) => {
          setDocs(data.map((r) => ({ ...r, kind: 'doc' as const })));
          setCursor(0);
        })
        .catch(() => {});
    }, 200);

    return () => { if (docTimer.current) clearTimeout(docTimer.current); };
  }, [query, domains]);

  const hasCtx7 = query.trim().length >= 2;
  const allResults: AnyResult[] = [
    ...docs,
    ...tables,
    ...(hasCtx7 ? [{ kind: 'ctx7' as const }] : []),
  ];
  const totalItems = allResults.length;

  function selectDoc(r: DocResult) {
    navigate({ to: `/${r.root}/${r.path.replace(/\.mdx?$/, '')}` });
    onClose();
  }

  function selectTable(r: TableResult) {
    navigate({ to: '/schema' });
    onClose();
  }

  function selectCtx7() {
    navigate({ to: '/ctx7', search: { q: query } as any });
    onClose();
  }

  function activate(item: AnyResult) {
    if (item.kind === 'doc')   selectDoc(item as DocResult);
    if (item.kind === 'table') selectTable(item as TableResult);
    if (item.kind === 'ctx7')  selectCtx7();
  }

  function handleKeyDown(e: React.KeyboardEvent) {
    if (e.key === 'ArrowDown') { e.preventDefault(); setCursor((c) => Math.min(c + 1, totalItems - 1)); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); setCursor((c) => Math.max(c - 1, 0)); }
    else if (e.key === 'Enter') {
      const item = allResults[cursor];
      if (item) activate(item);
    }
  }

  if (!open) return null;

  let idx = 0;

  return (
    <div className="search-modal-overlay" onClick={onClose}>
      <div className="search-modal" onClick={(e) => e.stopPropagation()}>
        <div className="search-modal-header">
          <svg className="search-modal-icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8">
            <circle cx="6.5" cy="6.5" r="4.5" /><line x1="10.5" y1="10.5" x2="14" y2="14" />
          </svg>
          <input
            ref={inputRef}
            className="search-modal-input"
            placeholder="Search docs, tables, library docs…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
          />
          {query && (
            <button className="search-modal-clear" onClick={() => { setQuery(''); inputRef.current?.focus(); }}>✕</button>
          )}
          <kbd className="search-modal-esc">esc</kbd>
        </div>

        {totalItems > 0 && (
          <div className="search-modal-results">
            {/* ── Docs ── */}
            {docs.length > 0 && (
              <>
                <div className="search-modal-group-label">Docs</div>
                {docs.map((r) => {
                  const i = idx++;
                  return (
                    <button key={`${r.root}/${r.path}`}
                      className={`search-modal-result${cursor === i ? ' active' : ''}`}
                      onClick={() => selectDoc(r)}
                      onMouseEnter={() => setCursor(i)}
                    >
                      <span className="search-modal-result-path">{r.root}/{r.path.replace(/\.mdx?$/, '')}</span>
                      {r.matches[0] && <span className="search-modal-result-match">{r.matches[0]}</span>}
                    </button>
                  );
                })}
              </>
            )}

            {/* ── Tables ── */}
            {tables.length > 0 && (
              <>
                <div className="search-modal-group-label">DB Tables</div>
                {tables.map((r) => {
                  const i = idx++;
                  return (
                    <button key={r.table}
                      className={`search-modal-result${cursor === i ? ' active' : ''}`}
                      onClick={() => selectTable(r)}
                      onMouseEnter={() => setCursor(i)}
                    >
                      <span className="search-modal-result-path" style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                        <span className="search-modal-dot" style={{ background: r.color }} />
                        {r.table}
                      </span>
                      <span className="search-modal-result-match">{r.group} → {r.domain}</span>
                    </button>
                  );
                })}
              </>
            )}

            {/* ── Context7 ── */}
            {hasCtx7 && (
              <>
                <div className="search-modal-group-label">Library Docs</div>
                {(() => { const i = idx++; return (
                  <button
                    className={`search-modal-result search-modal-ctx7${cursor === i ? ' active' : ''}`}
                    onClick={selectCtx7}
                    onMouseEnter={() => setCursor(i)}
                  >
                    <span className="search-modal-result-path">Search "{query}" in library docs via Context7 →</span>
                    <span className="search-modal-result-match">Browse React, Next.js, Prisma, and more</span>
                  </button>
                ); })()}
              </>
            )}
          </div>
        )}

        {query.trim().length >= 2 && docs.length === 0 && tables.length === 0 && (
          <div className="search-modal-empty">No matches in docs or tables</div>
        )}
      </div>
    </div>
  );
}
