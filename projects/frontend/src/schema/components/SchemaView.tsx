import { useState, useRef, useMemo, useCallback } from 'react';
import { computeLayout } from '../utils/layout';
import { buildDomainMap } from '../utils/utils';
import { useCamera } from '../hooks/useCamera';
import { useCardDrag } from '../hooks/useCardDrag';
import { useDomainFilter } from '../hooks/useDomainFilter';
import { useKeyboardShortcuts } from '../hooks/useKeyboardShortcuts';
import { useSearch } from '../hooks/useSearch';
import { EdgesSvg } from './EdgesSvg';
import { GroupBackgrounds } from './GroupBackgrounds';
import { TableCard } from './TableCard';
import { DetailPanel } from './DetailPanel';
import { ZoomControls } from './ZoomControls';
import { Toast } from './Toast';
import { DriftPanel } from './DriftPanel';
import { TableGrid } from './TableGrid';
import { Toolbar } from './Toolbar';
import type { Palette, Mode } from '../hooks/useTheme';

type ViewMode = 'browse' | 'canvas';

export function SchemaView({ data, palette, mode, onPaletteChange, onToggleMode }: {
  data: TabData;
  palette: Palette;
  mode: Mode;
  onPaletteChange: (p: Palette) => void;
  onToggleMode: () => void;
}) {
  const { tables, fks, domains, title, drift } = data;
  const [viewMode, setMode] = useState<ViewMode>('browse');
  const [selected, setSelected] = useState<string | null>(null);

  const { searchQuery, setSearchQuery, searchMatchSet } = useSearch(tables);
  const { activeDomains, legendItems, toggleDomain } = useDomainFilter(domains);

  function handleSelect(id: string) {
    setSelected((prev) => (prev === id ? null : id));
  }

  function handleGoTo(id: string) {
    setSelected(id);
    if (viewMode === 'canvas') {
      // focusNode handled inside canvas panel
    } else {
      setMode('browse');
    }
  }

  return (
    <>
      {/* ── Toolbar ───────────────────────────────────────────────────────── */}
      <Toolbar
        title={title}
        tableCount={tables.length}
        fkCount={fks.length}
        searchQuery={searchQuery}
        onSearchChange={setSearchQuery}
        legendItems={legendItems}
        activeDomains={activeDomains}
        onToggleDomain={toggleDomain}
        palette={palette}
        themeMode={mode}
        onPaletteChange={onPaletteChange}
        onToggleMode={onToggleMode}
        viewMode={viewMode}
        onViewModeChange={setMode}
      />

      {/* ── Browse mode ───────────────────────────────────────────────────── */}
      {viewMode === 'browse' && (
        <div className="schema-browse">
          <TableGrid
            tables={tables}
            fks={fks}
            domains={domains}
            selected={selected}
            onSelect={handleSelect}
            searchQuery={searchQuery}
            activeDomains={activeDomains}
          />
          <BrowseDetailPanel
            selectedId={selected}
            tables={tables}
            fks={fks}
            domains={domains}
            onClose={() => setSelected(null)}
            onGoTo={handleGoTo}
          />
        </div>
      )}

      {/* ── Canvas mode ───────────────────────────────────────────────────── */}
      {viewMode === 'canvas' && (
        <CanvasPanel
          data={data}
          searchQuery={searchQuery}
          searchMatchSet={searchMatchSet}
          activeDomains={activeDomains}
          selected={selected}
          onSelect={setSelected}
          onGoTo={handleGoTo}
          drift={drift}
        />
      )}
    </>
  );
}

// ── Browse detail panel ───────────────────────────────────────────────────────

function BrowseDetailPanel({ selectedId, tables, fks, domains, onClose, onGoTo }: {
  selectedId: string | null;
  tables: Table[];
  fks: FK[];
  domains: Domain[];
  onClose: () => void;
  onGoTo: (id: string) => void;
}) {
  const table = selectedId ? tables.find((t) => t.name === selectedId) ?? null : null;
  const domain = useMemo(() => {
    if (!selectedId) return null;
    for (const d of domains) if (d.tables.includes(selectedId)) return d;
    return null;
  }, [selectedId, domains]);

  const fkOut = fks.filter((f) => f.from === selectedId);
  const fkIn  = fks.filter((f) => f.to === selectedId);

  if (!table) return null;

  return (
    <div className="browse-detail">
      <div className="browse-detail-header">
        <div className="browse-detail-title">
          {domain && <span className="browse-detail-dot" style={{ background: domain.color }} />}
          <span>{table.name}</span>
          {domain && <span className="browse-detail-domain">{domain.group ?? domain.label}</span>}
        </div>
        <button className="browse-detail-close" onClick={onClose}>✕</button>
      </div>

      <div className="browse-detail-body">
        <div className="browse-detail-section-label">Columns ({table.columns.length})</div>
        {table.columns.map((col) => (
          <div key={col.name} className={`browse-col${col.fk ? ' browse-col-fk' : ''}`}>
            <span className="browse-col-badges">
              {col.pk && <span className="col-badge pk">PK</span>}
              {col.fk && <span className="col-badge fk">FK</span>}
              {!col.pk && !col.fk && <span className="col-badge none">  </span>}
            </span>
            <span className="browse-col-name">{col.name}</span>
            <span className="browse-col-type">{col.type}</span>
            {col.fkTarget && (
              <button className="browse-col-target" onClick={() => onGoTo(col.fkTarget!)}>
                → {col.fkTarget}
              </button>
            )}
          </div>
        ))}

        {fkIn.length > 0 && (
          <>
            <div className="browse-detail-section-label" style={{ marginTop: '1rem' }}>Referenced by ({fkIn.length})</div>
            {fkIn.map((f) => (
              <button key={`${f.from}.${f.fromCol}`} className="browse-rel-item" onClick={() => onGoTo(f.from)}>
                ← {f.from} <span className="browse-rel-via">via {f.fromCol}</span>
              </button>
            ))}
          </>
        )}

        {fkOut.length > 0 && (
          <>
            <div className="browse-detail-section-label" style={{ marginTop: '1rem' }}>References ({fkOut.length})</div>
            {fkOut.map((f) => (
              <button key={`${f.from}.${f.fromCol}`} className="browse-rel-item" onClick={() => onGoTo(f.to)}>
                → {f.to} <span className="browse-rel-via">via {f.fromCol}</span>
              </button>
            ))}
          </>
        )}
      </div>
    </div>
  );
}

// ── Canvas panel (existing behavior) ─────────────────────────────────────────

function CanvasPanel({ data, searchQuery, searchMatchSet, activeDomains, selected, onSelect, onGoTo, drift }: {
  data: TabData;
  searchQuery: string;
  searchMatchSet: Set<string> | null;
  activeDomains: Set<string>;
  selected: string | null;
  onSelect: (id: string) => void;
  onGoTo: (id: string) => void;
  drift?: DriftReport;
}) {
  const { tables, fks, domains } = data;
  const layout = useMemo(() => computeLayout(tables, fks, domains), [tables, fks, domains]);
  const { nodes, nodeMap, edges, groups, adjT, wW, wH } = layout;

  const [hovered, setHovered] = useState<string | null>(null);
  const viewportRef = useRef<HTMLDivElement>(null);

  const { worldRef, cam, fitAll, focusNode, zoomBy } = useCamera({
    viewportRef, wW, wH,
    onBackgroundClick: () => onSelect('')
  });

  const { handleCardPointerDown, handleCardPointerMove, handleCardPointerUp } = useCardDrag({
    viewportRef, cam, edges, focusNode,
    setSelected: onSelect as (val: string | ((prev: string | null) => string | null)) => void,
  });

  const domainOf = useMemo(() => buildDomainMap(domains), [domains]);

  const isDimmed = useCallback(
    (id: string) => {
      const domain = domainOf[id];
      if (domain && !activeDomains.has(domain.key)) return true;
      if (searchMatchSet && !searchMatchSet.has(id)) return true;
      return selected ? id !== selected && !adjT[selected]?.has(id) : false;
    },
    [selected, searchMatchSet, activeDomains, adjT, domainOf]
  );

  const focusAndSelect = useCallback(
    (id: string) => {
      onSelect(id);
      if (nodeMap[id]) focusNode(nodeMap[id]);
    },
    [focusNode, nodeMap, onSelect]
  );

  useKeyboardShortcuts({ onClear: () => onSelect('') });

  const selectedNode = selected ? nodeMap[selected] : null;

  return (
    <div id="viewport" ref={viewportRef}>
      <div id="world" ref={worldRef}>
        <EdgesSvg edges={edges} width={wW} height={wH} isDimmed={isDimmed} selectedId={selected} hoveredId={hovered} />
        <GroupBackgrounds groups={groups} />
        {nodes.map((node) => {
          const dim = isDimmed(node.id);
          const isConnected = !selected && !dim && adjT[hovered!]?.has(node.id);
          return (
            <TableCard
              key={node.id} node={node}
              isDimmed={dim}
              isSelected={selected === node.id}
              isConnected={!!isConnected}
              isSearchMatch={!!searchMatchSet?.has(node.id)}
              onHover={() => setHovered(node.id)}
              onLeave={() => setHovered(null)}
              onPointerDown={(e) => handleCardPointerDown(e, node)}
              onPointerMove={(e) => handleCardPointerMove(e, node)}
              onPointerUp={(e) => handleCardPointerUp(e, node)}
            />
          );
        })}
      </div>

      <DetailPanel selectedId={selected} selectedNode={selectedNode} edges={edges} nodeMap={nodeMap}
        onClose={() => onSelect('')} onGoTo={focusAndSelect} />
      <ZoomControls onZoomIn={() => zoomBy(1.4)} onZoomOut={() => zoomBy(1 / 1.4)} onFitAll={() => fitAll(true)} />
      <Toast />
      {drift && <DriftPanel drift={drift} onGoTo={focusAndSelect} />}
    </div>
  );
}
