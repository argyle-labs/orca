<script lang="ts">
  import {
    computeLayout, edgePath, MAX_VISIBLE_COLS,
    type Col, type Table, type FK, type Domain, type TableNode, type Edge,
  } from '$lib/schema/layout';
  import '../../schema/css/schemaVisualizer.css';

  // ── Data ────────────────────────────────────────────────────────────────────
  let { data } = $props();

  type ApiCol = { name: string; type: string; key?: string; nullable?: boolean; fk_target?: string | null };
  type ApiTab = {
    title?: string;
    tables?: Array<{ name: string } | string>;
    columns?: Record<string, ApiCol[]>;
    foreignKeys?: Array<{ table: string; column: string; refTable: string }>;
    domains?: unknown[];
    drift?: DriftReport;
  };
  interface ConstraintDrift {
    missingDbConstraints: { table: string; column: string; target: string }[];
    extraDbConstraints: { table: string; column: string; referencedTable: string }[];
    mismatchedFkTargets: { table: string; column: string; configTarget: string; actualTarget: string }[];
  }
  interface DriftReport {
    unassignedTables: string[];
    ghostTables: string[];
    unmappedFkColumns: { table: string; column: string }[];
    invalidFkTargets: { column: string; target: string }[];
    constraintDrift?: ConstraintDrift;
    totalIssues: number;
  }
  interface TabData {
    title: string;
    tables: Table[];
    fks: FK[];
    domains: Domain[];
    drift?: DriftReport;
  }

  function normalizeTab(raw: ApiTab): TabData {
    const colMap = raw.columns ?? {};
    const seen = new Set<string>();
    const tables: Table[] = [];
    for (const t of raw.tables ?? []) {
      const name = typeof t === 'string' ? t : t.name;
      if (seen.has(name)) continue;
      seen.add(name);
      const columns: Col[] = (colMap[name] ?? []).map((c) => ({
        name: c.name, type: c.type, pk: c.key === 'PRI', fk: !!c.fk_target,
        fkTarget: c.fk_target ?? null,
      }));
      tables.push({ name, columns });
    }
    const seenFk = new Set<string>();
    const fks: FK[] = [];
    for (const fk of raw.foreignKeys ?? []) {
      const key = `${fk.table}.${fk.column}->${fk.refTable}`;
      if (seenFk.has(key)) continue;
      seenFk.add(key);
      fks.push({ from: fk.table, fromCol: fk.column, to: fk.refTable });
    }
    return { title: raw.title ?? '', tables, fks, domains: (raw.domains ?? []) as Domain[], drift: raw.drift };
  }

  const tabs = $derived.by<TabData[]>(() =>
    ((data?.schema?.tabs ?? []) as ApiTab[]).map(normalizeTab).filter((t) => t.tables.length > 0)
  );
  const showTabs = $derived(tabs.length > 1);

  // ── Global state ─────────────────────────────────────────────────────────────
  let activeTab = $state(0);
  let mode = $state<'browse' | 'canvas'>('browse');
  let searchQuery = $state('');
  let selected = $state<string | null>(null);

  const tab = $derived(tabs[activeTab] ?? null);

  $effect(() => { void activeTab; selected = null; });
  $effect(() => { void mode; selected = null; });

  // ── Domain filter ────────────────────────────────────────────────────────────
  let activeDomainKeys = $state<Set<string>>(new Set());
  $effect(() => {
    if (tab) activeDomainKeys = new Set(tab.domains.map((d) => d.key));
  });

  function toggleDomain(keys: string[]) {
    const next = new Set(activeDomainKeys);
    if (keys.every((k) => next.has(k))) keys.forEach((k) => next.delete(k));
    else keys.forEach((k) => next.add(k));
    activeDomainKeys = next;
  }

  type LegendItem = { key: string; label: string; color: string; groupKeys: string[] };
  const legendItems = $derived.by<LegendItem[]>(() => {
    if (!tab) return [];
    const seen = new Set<string>();
    const items: LegendItem[] = [];
    for (const d of tab.domains) {
      const gKey = d.group ?? d.key;
      if (seen.has(gKey)) {
        items.find((i) => i.key === gKey)?.groupKeys.push(d.key);
      } else {
        seen.add(gKey);
        items.push({ key: gKey, label: d.group ?? d.label, color: d.color, groupKeys: [d.key] });
      }
    }
    return items;
  });

  // ── Search ───────────────────────────────────────────────────────────────────
  const searchMatchSet = $derived.by<Set<string> | null>(() => {
    if (!tab || !searchQuery.trim()) return null;
    const q = searchQuery.toLowerCase();
    const s = new Set<string>();
    for (const t of tab.tables) {
      if (t.name.toLowerCase().includes(q) || t.columns.some((c) => c.name.toLowerCase().includes(q))) s.add(t.name);
    }
    return s;
  });

  // ── Domain map ───────────────────────────────────────────────────────────────
  const domainOf = $derived.by<Record<string, Domain>>(() => {
    if (!tab) return {};
    const m: Record<string, Domain> = {};
    for (const d of tab.domains) for (const t of d.tables) m[t] = d;
    return m;
  });

  // ── Browse: filtered tables ──────────────────────────────────────────────────
  const filteredTables = $derived.by<Table[]>(() => {
    if (!tab) return [];
    const q = searchQuery.toLowerCase();
    return tab.tables.filter((t) => {
      const d = domainOf[t.name];
      if (d && !activeDomainKeys.has(d.key)) return false;
      if (!q) return true;
      return t.name.toLowerCase().includes(q) || t.columns.some((c) => c.name.toLowerCase().includes(q));
    });
  });

  // ── Browse: hierarchy ────────────────────────────────────────────────────────
  type HierarchyEntry = { domain: Domain; tables: Table[] };
  type SubgroupEntry = { key: string; label: string; entries: HierarchyEntry[] };
  type GroupEntry = { key: string; label: string; color: string; subgroups: SubgroupEntry[]; total: number };

  const hierarchy = $derived.by<GroupEntry[]>(() => {
    if (!tab) return [];
    const groupOrder: string[] = [];
    const groupSeen = new Set<string>();
    for (const d of tab.domains) {
      const g = d.group ?? d.key;
      if (!groupSeen.has(g)) { groupSeen.add(g); groupOrder.push(g); }
    }
    const result: GroupEntry[] = [];
    for (const gKey of groupOrder) {
      const gDomains = tab.domains.filter((d) => (d.group ?? d.key) === gKey);
      const sgOrder: string[] = [];
      const sgSeen = new Set<string>();
      for (const d of gDomains) {
        const sg = (d as any).subgroup ?? '__';
        if (!sgSeen.has(sg)) { sgSeen.add(sg); sgOrder.push(sg); }
      }
      const subgroups: SubgroupEntry[] = [];
      for (const sgKey of sgOrder) {
        const sgDomains = gDomains.filter((d) => ((d as any).subgroup ?? '__') === sgKey);
        const entries: HierarchyEntry[] = sgDomains
          .map((d) => ({ domain: d, tables: filteredTables.filter((t) => domainOf[t.name]?.key === d.key) }))
          .filter((e) => e.tables.length > 0);
        if (entries.length > 0) subgroups.push({ key: sgKey, label: sgKey === '__' ? '' : sgKey, entries });
      }
      const total = subgroups.reduce((s, sg) => s + sg.entries.reduce((s2, e) => s2 + e.tables.length, 0), 0);
      if (total > 0) result.push({ key: gKey, label: gKey, color: gDomains[0].color, subgroups, total });
    }
    const uncat = filteredTables.filter((t) => !domainOf[t.name]);
    if (uncat.length > 0) {
      const d: Domain = { key: '__other', label: 'Other', color: '#556', tables: [] };
      result.push({ key: '__other', label: 'Other', color: '#556',
        subgroups: [{ key: '__', label: '', entries: [{ domain: d, tables: uncat }] }], total: uncat.length });
    }
    return result;
  });

  let collapsedGroups = $state(new Set<string>());
  let collapsedDomains = $state(new Set<string>());
  $effect(() => { void activeTab; collapsedGroups = new Set(); collapsedDomains = new Set(); });

  function toggleGroup(key: string) {
    const next = new Set(collapsedGroups);
    if (next.has(key)) next.delete(key); else next.add(key);
    collapsedGroups = next;
  }
  function toggleDomainSection(key: string) {
    const next = new Set(collapsedDomains);
    if (next.has(key)) next.delete(key); else next.add(key);
    collapsedDomains = next;
  }

  const fkOutCount = $derived.by<Record<string, number>>(() => {
    if (!tab) return {};
    const c: Record<string, number> = {};
    for (const fk of tab.fks) c[fk.from] = (c[fk.from] ?? 0) + 1;
    return c;
  });
  const fkInCount = $derived.by<Record<string, number>>(() => {
    if (!tab) return {};
    const c: Record<string, number> = {};
    for (const fk of tab.fks) c[fk.to] = (c[fk.to] ?? 0) + 1;
    return c;
  });

  // ── Browse: detail ───────────────────────────────────────────────────────────
  const browseTable = $derived(tab?.tables.find((t) => t.name === selected) ?? null);
  const browseDomain = $derived.by<Domain | null>(() => {
    if (!selected || !tab) return null;
    for (const d of tab.domains) if (d.tables.includes(selected)) return d;
    return null;
  });
  const browseFkOut = $derived(tab?.fks.filter((f) => f.from === selected) ?? []);
  const browseFkIn = $derived(tab?.fks.filter((f) => f.to === selected) ?? []);

  function handleSelect(id: string) { selected = selected === id ? null : id; }
  function handleGoTo(id: string) {
    selected = id;
    if (mode === 'canvas') canvasFocusId(id);
    else mode = 'browse';
  }

  // ── Canvas: layout ───────────────────────────────────────────────────────────
  const layout = $derived.by(() => {
    if (!tab || mode !== 'canvas') return null;
    return computeLayout(tab.tables, tab.fks, tab.domains);
  });

  // ── Canvas: camera state ──────────────────────────────────────────────────────
  let viewportEl: HTMLDivElement | null = null;
  let worldEl = $state<HTMLDivElement | null>(null);
  const cam = { x: 0, y: 0, z: 1 };

  function applyTransform() {
    if (worldEl) worldEl.style.transform = `scale(${cam.z}) translate(${-cam.x}px,${-cam.y}px)`;
  }
  function easeIO(t: number) { return t < 0.5 ? 2 * t * t : -1 + (4 - 2 * t) * t; }
  function animateCam(tx: number, ty: number, tz: number, dur = 300) {
    const { x: sx, y: sy, z: sz } = cam;
    const t0 = performance.now();
    (function step(now: number) {
      const p = Math.min((now - t0) / dur, 1), e = easeIO(p);
      cam.x = sx + (tx - sx) * e; cam.y = sy + (ty - sy) * e; cam.z = sz + (tz - sz) * e;
      applyTransform();
      if (p < 1) requestAnimationFrame(step);
    })(performance.now());
  }
  function clampZ(z: number) { return Math.min(Math.max(z, 0.05), 4); }

  function fitAll(animate = true) {
    const vp = viewportEl, lay = layout;
    if (!vp || !lay) return;
    const z = Math.min(vp.clientWidth / lay.wW, vp.clientHeight / lay.wH) * 0.9;
    const tx = (lay.wW * z - vp.clientWidth) / 2 / z;
    const ty = (lay.wH * z - vp.clientHeight) / 2 / z;
    if (animate) animateCam(tx, ty, z);
    else { Object.assign(cam, { x: tx, y: ty, z }); applyTransform(); }
  }
  function canvasFocusId(id: string) {
    const vp = viewportEl, lay = layout;
    if (!vp || !lay) return;
    const node = lay.nodeMap[id];
    if (!node) return;
    const tz = Math.max(cam.z, 0.8);
    animateCam(node.x + node.w / 2 - vp.clientWidth / 2 / tz, node.y + node.h / 2 - vp.clientHeight / 2 / tz, tz);
  }
  function zoomBy(factor: number) {
    const vp = viewportEl;
    if (!vp) return;
    const cx = vp.clientWidth / 2 / cam.z + cam.x, cy = vp.clientHeight / 2 / cam.z + cam.y;
    const tz = clampZ(cam.z * factor);
    animateCam(cx - vp.clientWidth / 2 / tz, cy - vp.clientHeight / 2 / tz, tz, 200);
  }

  // ── Canvas: viewport action (pan + zoom) ─────────────────────────────────────
  function viewportAction(vp: HTMLDivElement) {
    viewportEl = vp;
    requestAnimationFrame(() => fitAll(false));

    let isPanning = false, hasMoved = false;
    let panStart = { x: 0, y: 0 }, camStart = { x: 0, y: 0 };

    function onDown(ev: PointerEvent) {
      const t = ev.target as HTMLElement;
      if (t !== vp && t !== worldEl && t.tagName !== 'svg' && t.id !== 'edges-svg') return;
      isPanning = true; hasMoved = false;
      panStart = { x: ev.clientX, y: ev.clientY }; camStart = { x: cam.x, y: cam.y };
      vp.classList.add('grabbing'); vp.setPointerCapture(ev.pointerId);
    }
    function onMove(ev: PointerEvent) {
      if (!isPanning) return;
      if (Math.abs(ev.clientX - panStart.x) > 3 || Math.abs(ev.clientY - panStart.y) > 3) hasMoved = true;
      cam.x = camStart.x - (ev.clientX - panStart.x) / cam.z;
      cam.y = camStart.y - (ev.clientY - panStart.y) / cam.z;
      applyTransform();
    }
    function onUp(ev: PointerEvent) {
      if (!isPanning) return;
      isPanning = false; vp.classList.remove('grabbing'); vp.releasePointerCapture(ev.pointerId);
      if (!hasMoved) selected = null;
    }
    function onWheel(ev: WheelEvent) {
      ev.preventDefault();
      const rect = vp.getBoundingClientRect();
      const delta = ev.ctrlKey ? -ev.deltaY * 0.01 : -ev.deltaY * 0.002;
      const newZ = clampZ(cam.z * Math.pow(2, delta));
      const mx = ev.clientX - rect.left, my = ev.clientY - rect.top;
      cam.x = mx / cam.z + cam.x - mx / newZ;
      cam.y = my / cam.z + cam.y - my / newZ;
      cam.z = newZ; applyTransform();
    }
    vp.addEventListener('pointerdown', onDown);
    vp.addEventListener('pointermove', onMove);
    vp.addEventListener('pointerup', onUp);
    vp.addEventListener('wheel', onWheel, { passive: false });

    return {
      destroy() {
        vp.removeEventListener('pointerdown', onDown);
        vp.removeEventListener('pointermove', onMove);
        vp.removeEventListener('pointerup', onUp);
        vp.removeEventListener('wheel', onWheel);
        viewportEl = null;
      }
    };
  }

  $effect(() => {
    // re-fit when layout changes (e.g. tab switch to canvas)
    if (layout && viewportEl) requestAnimationFrame(() => fitAll(false));
  });

  // ── Canvas: card drag ─────────────────────────────────────────────────────────
  type DragState = { node: TableNode; ox: number; oy: number; moved: boolean };
  let dragState: DragState | null = null;
  let clickState = { time: 0, id: '' };

  function cardDown(ev: PointerEvent, node: TableNode) {
    ev.stopPropagation();
    const vp = viewportEl; if (!vp) return;
    const rect = vp.getBoundingClientRect();
    const wx = (ev.clientX - rect.left) / cam.z + cam.x;
    const wy = (ev.clientY - rect.top) / cam.z + cam.y;
    dragState = { node, ox: wx - node.x, oy: wy - node.y, moved: false };
    (ev.currentTarget as HTMLElement).setPointerCapture(ev.pointerId);
    (ev.currentTarget as HTMLElement).classList.add('dragging');
  }
  function cardMove(ev: PointerEvent, node: TableNode) {
    const ds = dragState; if (!ds || ds.node !== node) return;
    const vp = viewportEl; if (!vp) return;
    const rect = vp.getBoundingClientRect();
    const nx = (ev.clientX - rect.left) / cam.z + cam.x - ds.ox;
    const ny = (ev.clientY - rect.top) / cam.z + cam.y - ds.oy;
    if (!ds.moved && (Math.abs(nx - node.x) > 3 || Math.abs(ny - node.y) > 3)) ds.moved = true;
    if (!ds.moved) return;
    node.x = nx; node.y = ny;
    const el = ev.currentTarget as HTMLElement;
    el.style.left = `${nx}px`; el.style.top = `${ny}px`;
    layout?.edges.forEach((edge, i) => {
      if (edge.source.id !== node.id && edge.target.id !== node.id) return;
      const ep = edgePath(edge.source, edge.target);
      const g = document.querySelector(`[data-edge="${i}"]`);
      if (!g) return;
      g.querySelector('path')?.setAttribute('d', ep.d);
      g.querySelector('polygon')?.setAttribute('points', ep.arrow);
      const lbl = g.querySelector('text');
      if (lbl) { lbl.setAttribute('x', String(ep.mx)); lbl.setAttribute('y', String(ep.my - 6)); }
    });
  }
  function cardUp(ev: PointerEvent, node: TableNode) {
    const ds = dragState; if (!ds || ds.node !== node) return;
    const el = ev.currentTarget as HTMLElement;
    el.classList.remove('dragging'); el.releasePointerCapture(ev.pointerId);
    if (!ds.moved) {
      const now = Date.now();
      if (now - clickState.time < 350 && clickState.id === node.id) {
        canvasFocusId(node.id); selected = node.id; clickState = { time: 0, id: '' };
      } else {
        selected = selected === node.id ? null : node.id; clickState = { time: now, id: node.id };
      }
    }
    dragState = null;
  }

  // ── Canvas: dimming ──────────────────────────────────────────────────────────
  let hovered = $state<string | null>(null);

  function isDimmed(id: string): boolean {
    const d = domainOf[id];
    if (d && !activeDomainKeys.has(d.key)) return true;
    if (searchMatchSet && !searchMatchSet.has(id)) return true;
    if (selected && layout) return id !== selected && !(layout.adjT[selected]?.has(id));
    return false;
  }

  // ── Canvas: selected node detail ─────────────────────────────────────────────
  const canvasNode = $derived(layout?.nodeMap[selected ?? ''] ?? null);
  const canvasEdgesOut = $derived(layout?.edges.filter((e) => e.source.id === selected) ?? []);
  const canvasEdgesIn = $derived(layout?.edges.filter((e) => e.target.id === selected) ?? []);

  // ── Drift ─────────────────────────────────────────────────────────────────────
  let driftOpen = $state(false);
</script>

<svelte:head><title>Schema — orca</title></svelte:head>

{#if !tab}
  <div style="display:flex;align-items:center;justify-content:center;height:100%;color:var(--color-text-dim)">
    No schema data. Configure a database in System → Schema.
  </div>
{:else}
  <div class="schema-page{showTabs ? ' has-tabs' : ''}">

    {#if showTabs}
      <div id="tab-bar">
        {#each tabs as t, i}
          <button class="tab-item{activeTab === i ? ' active' : ''}" onclick={() => (activeTab = i)}>{t.title || `Tab ${i + 1}`}</button>
        {/each}
      </div>
    {/if}

    <div id="toolbar">
      <h1>{tab.title}</h1>
      <span class="stats">{tab.tables.length} tables &middot; {tab.fks.length} relations</span>
      <input type="text" id="search" placeholder="Search tables, columns…" bind:value={searchQuery} />
      <div class="legend">
        {#each legendItems as item}
          <div
            class="legend-item{!item.groupKeys.some((k) => activeDomainKeys.has(k)) ? ' dimmed' : ''}"
            role="button" tabindex="0"
            onclick={() => toggleDomain(item.groupKeys)}
            onkeydown={(e) => e.key === 'Enter' && toggleDomain(item.groupKeys)}
          >
            <div class="legend-dot" style="background:{item.color}"></div>
            {item.label}
          </div>
        {/each}
      </div>
      <div class="schema-mode-toggle">
        <button class="mode-btn{mode === 'browse' ? ' active' : ''}" onclick={() => (mode = 'browse')} title="Browse tables">☰</button>
        <button class="mode-btn{mode === 'canvas' ? ' active' : ''}" onclick={() => (mode = 'canvas')} title="Canvas view">⊞</button>
      </div>
    </div>

    <!-- ── Browse ─────────────────────────────────────────────────────────── -->
    {#if mode === 'browse'}
      <div class="schema-browse">
        <div class="tg-scroll">
          {#if filteredTables.length === 0}
            <div class="tg-empty">No tables match your search or filter.</div>
          {:else}
            {#each hierarchy as group}
              <section class="tg-group">
                <button class="tg-group-header" onclick={() => toggleGroup(group.key)}>
                  <span class="tg-group-label">{group.label}</span>
                  <span class="tg-group-count">{group.total}</span>
                  <span class="tg-group-chevron">{collapsedGroups.has(group.key) ? '▸' : '▾'}</span>
                </button>
                {#if !collapsedGroups.has(group.key)}
                  <div class="tg-group-body">
                    {#each group.subgroups as sg}
                      <div class={sg.label ? 'tg-subgroup' : ''}>
                        {#if sg.label}<div class="tg-subgroup-header">{sg.label}</div>{/if}
                        {#each sg.entries as { domain, tables: domTables }}
                          <section class="tg-section">
                            <button class="tg-section-header" onclick={() => toggleDomainSection(domain.key)} style="border-color:{domain.color}">
                              <span class="tg-section-dot" style="background:{domain.color}"></span>
                              <span class="tg-section-label">{domain.label}</span>
                              <span class="tg-section-count">{domTables.length}</span>
                              <span class="tg-section-chevron">{collapsedDomains.has(domain.key) ? '▸' : '▾'}</span>
                            </button>
                            {#if !collapsedDomains.has(domain.key)}
                              <div class="tg-grid">
                                {#each domTables as table}
                                  {@const pkCount = table.columns.filter((c) => c.pk).length}
                                  {@const fkCount = table.columns.filter((c) => c.fk).length}
                                  {@const inCount = fkInCount[table.name] ?? 0}
                                  <button
                                    class="tg-card{selected === table.name ? ' tg-card-selected' : ''}"
                                    style="--domain-color:{domain.color}"
                                    onclick={() => handleSelect(table.name)}
                                  >
                                    <div class="tg-card-header">
                                      <span class="tg-card-name">{table.name}</span>
                                      <span class="tg-card-cols">{table.columns.length} cols</span>
                                    </div>
                                    <div class="tg-card-meta">
                                      {#if pkCount > 0}<span class="tg-badge tg-badge-pk">PK·{pkCount}</span>{/if}
                                      {#if fkCount > 0}<span class="tg-badge tg-badge-fk">FK·{fkCount}</span>{/if}
                                      {#if inCount > 0}<span class="tg-badge tg-badge-ref">←{inCount}</span>{/if}
                                    </div>
                                  </button>
                                {/each}
                              </div>
                            {/if}
                          </section>
                        {/each}
                      </div>
                    {/each}
                  </div>
                {/if}
              </section>
            {/each}
          {/if}
        </div>

        {#if browseTable}
          {@const t = browseTable}
          {@const dom = browseDomain}
          <div class="browse-detail">
            <div class="browse-detail-header">
              <div class="browse-detail-title">
                {#if dom}<span class="browse-detail-dot" style="background:{dom.color}"></span>{/if}
                <span>{t.name}</span>
                {#if dom}<span class="browse-detail-domain">{dom.group ?? dom.label}</span>{/if}
              </div>
              <button class="browse-detail-close" onclick={() => (selected = null)}>✕</button>
            </div>
            <div class="browse-detail-body">
              <div class="browse-detail-section-label">Columns ({t.columns.length})</div>
              {#each t.columns as col}
                <div class="browse-col{col.fk ? ' browse-col-fk' : ''}">
                  <span class="browse-col-badges">
                    {#if col.pk}<span class="col-badge pk">PK</span>
                    {:else if col.fk}<span class="col-badge fk">FK</span>
                    {:else}<span class="col-badge none">  </span>{/if}
                  </span>
                  <span class="browse-col-name">{col.name}</span>
                  <span class="browse-col-type">{col.type}</span>
                  {#if col.fkTarget}
                    <button class="browse-col-target" onclick={() => handleGoTo(col.fkTarget!)}>→ {col.fkTarget}</button>
                  {/if}
                </div>
              {/each}
              {#if browseFkIn.length > 0}
                <div class="browse-detail-section-label" style="margin-top:1rem">Referenced by ({browseFkIn.length})</div>
                {#each browseFkIn as f}
                  <button class="browse-rel-item" onclick={() => handleGoTo(f.from)}>← {f.from} <span class="browse-rel-via">via {f.fromCol}</span></button>
                {/each}
              {/if}
              {#if browseFkOut.length > 0}
                <div class="browse-detail-section-label" style="margin-top:1rem">References ({browseFkOut.length})</div>
                {#each browseFkOut as f}
                  <button class="browse-rel-item" onclick={() => handleGoTo(f.to)}>→ {f.to} <span class="browse-rel-via">via {f.fromCol}</span></button>
                {/each}
              {/if}
            </div>
          </div>
        {/if}
      </div>

    <!-- ── Canvas ─────────────────────────────────────────────────────────── -->
    {:else if layout}
      {@const { nodes, edges, groups, adjT, wW, wH } = layout}
      <div id="viewport" use:viewportAction>
        <div id="world" bind:this={worldEl}>

          <!-- Edges -->
          <svg id="edges-svg" width={wW} height={wH}>
            {#each edges as edge, i}
              {@const ep = edgePath(edge.source, edge.target)}
              {@const hi = selected === edge.source.id || selected === edge.target.id}
              {@const ho = !selected && (hovered === edge.source.id || hovered === edge.target.id)}
              {@const dim = isDimmed(edge.source.id) || isDimmed(edge.target.id)}
              <g data-edge={i}>
                <path d={ep.d} class="{hi ? 'highlight' : ho ? 'hover' : ''}{dim ? ' dim' : ''}" />
                <polygon points={ep.arrow} class="edge-arrow {hi ? 'highlight' : ho ? 'hover' : ''}{dim ? ' dim' : ''}" />
                <text x={ep.mx} y={ep.my - 6} text-anchor="middle" class="edge-label{hi || ho ? ' show' : ''}">{edge.col}</text>
              </g>
            {/each}
          </svg>

          <!-- Group backgrounds -->
          {#each groups as group}
            <div
              class="group-bg"
              style="left:{group.x}px;top:{group.y}px;width:{group.w}px;height:{group.h}px;border-color:color-mix(in srgb,{group.color} 35%,transparent);background:color-mix(in srgb,{group.color} 6%,transparent)"
            >
              <div class="group-label" style="color:{group.color}">{group.label}</div>
            </div>
            {#each group.subs as block}
              <div
                class="domain-bg"
                style="left:{block.x}px;top:{block.y}px;width:{block.w}px;height:{block.h}px;border-color:color-mix(in srgb,{block.domain.color} 40%,transparent)"
              >
                <div class="domain-label" style="color:{block.domain.color}">{block.domain.label}</div>
              </div>
            {/each}
          {/each}

          <!-- Table cards -->
          {#each nodes as node}
            {@const dim = isDimmed(node.id)}
            {@const isSel = selected === node.id}
            {@const isConn = !selected && !dim && hovered != null && adjT[hovered]?.has(node.id)}
            {@const isMatch = !!searchMatchSet?.has(node.id)}
            {@const visCols = node.table.columns.slice(0, MAX_VISIBLE_COLS)}
            <div
              class="table-card{dim ? ' dim' : ''}{isSel ? ' selected' : ''}{isConn ? ' connected' : ''}{isMatch ? ' search-match' : ''}"
              style="left:{node.x}px;top:{node.y}px;width:{node.w}px;border-top:3px solid {node.domain?.color ?? 'var(--border)'}"
              onpointerdown={(e) => cardDown(e, node)}
              onpointermove={(e) => cardMove(e, node)}
              onpointerup={(e) => cardUp(e, node)}
              onmouseenter={() => (hovered = node.id)}
              onmouseleave={() => (hovered = null)}
              role="button" tabindex="0"
            >
              <div class="table-header">
                <span class="name">{node.table.name}</span>
                <span class="count">{node.table.columns.length}</span>
              </div>
              <div class="table-cols">
                {#each visCols as col}
                  <div class="table-col">
                    {#if col.pk}<span class="col-badge pk">PK</span>
                    {:else if col.fk}<span class="col-badge fk">FK</span>
                    {:else}<span class="col-badge none"></span>{/if}
                    <span class="col-name{col.pk ? ' is-pk' : ''}{col.fk ? ' is-fk' : ''}">{col.name}</span>
                    <span class="col-type">{col.type}</span>
                  </div>
                {/each}
                {#if node.table.columns.length > MAX_VISIBLE_COLS}
                  <div class="table-more">+{node.table.columns.length - MAX_VISIBLE_COLS} more</div>
                {/if}
              </div>
            </div>
          {/each}
        </div>

        <!-- Canvas detail panel -->
        {#if canvasNode}
          {@const sn = canvasNode}
          <div id="detail" class="open">
            <div id="detail-header">
              <div style="display:flex;align-items:center;gap:8px">
                <h2>{sn.table.name}</h2>
                {#if sn.domain}
                  <span class="domain-badge" style="background:color-mix(in srgb,{sn.domain.color} 18%,transparent);color:{sn.domain.color}">{sn.domain.label}</span>
                {/if}
              </div>
              <button id="detail-close" onclick={() => (selected = null)}>&times;</button>
            </div>
            {#each sn.table.columns as col}
              {#if col.fk && col.fkTarget}
                <button class="detail-col fk-row" onclick={() => handleGoTo(col.fkTarget!)}>
                  <span class="col-badge-d fk">FK</span>
                  <span class="col-name-d">{col.name}</span>
                  <span class="col-type-d">{col.type}</span>
                  <span class="col-arrow">→</span>
                </button>
              {:else}
                <div class="detail-col{col.fk ? ' fk-row' : ''}">
                  {#if col.pk}<span class="col-badge-d pk">PK</span>{:else if col.fk}<span class="col-badge-d fk">FK</span>{/if}
                  <span class="col-name-d">{col.name}</span>
                  <span class="col-type-d">{col.type}</span>
                  {#if col.fk}<span class="col-arrow">→</span>{/if}
                </div>
              {/if}
            {/each}
            {#if canvasEdgesOut.length > 0 || canvasEdgesIn.length > 0}
              <div id="detail-relations">
                {#if canvasEdgesOut.length > 0}
                  <h3>References</h3>
                  {#each canvasEdgesOut as e}
                    <div class="relation-item" role="button" tabindex="0"
                      onclick={() => handleGoTo(e.target.id)}
                      onkeydown={(ev) => ev.key === 'Enter' && handleGoTo(e.target.id)}
                    >
                      <span class="relation-dir">→</span>
                      <span class="relation-table">{e.target.id}</span>
                      <span class="relation-via">via {e.col}</span>
                    </div>
                  {/each}
                {/if}
                {#if canvasEdgesIn.length > 0}
                  <h3>Referenced by</h3>
                  {#each canvasEdgesIn as e}
                    <div class="relation-item" role="button" tabindex="0"
                      onclick={() => handleGoTo(e.source.id)}
                      onkeydown={(ev) => ev.key === 'Enter' && handleGoTo(e.source.id)}
                    >
                      <span class="relation-dir">←</span>
                      <span class="relation-table">{e.source.id}</span>
                      <span class="relation-via">via {e.col}</span>
                    </div>
                  {/each}
                {/if}
              </div>
            {/if}
          </div>
        {/if}

        <!-- Zoom controls -->
        <div id="zoom-controls">
          <button onclick={() => zoomBy(1.4)} title="Zoom in">+</button>
          <button onclick={() => fitAll(true)} title="Fit all">⊡</button>
          <button onclick={() => zoomBy(1 / 1.4)} title="Zoom out">−</button>
        </div>

        <!-- Drift panel -->
        {#if tab.drift}
          {@const drift = tab.drift}
          {#if drift.totalIssues === 0}
            <button id="drift-btn" class="clean" title="No config drift">&#x2713;</button>
          {:else}
            <button id="drift-btn" class="has-issues" onclick={() => (driftOpen = !driftOpen)}>&#x26A0; {drift.totalIssues}</button>
            <div id="drift-panel" class={driftOpen ? 'open' : ''}>
              <div id="drift-header">
                <h2>Config Drift</h2>
                <button id="drift-close" onclick={() => (driftOpen = false)}>&times;</button>
              </div>
              {#if drift.unassignedTables.length > 0}
                <div class="drift-section">
                  <h3 class="drift-section-title amber">Unassigned Tables ({drift.unassignedTables.length})</h3>
                  <p class="drift-section-desc">In database but not in any domain</p>
                  {#each drift.unassignedTables as name}
                    <button class="drift-item clickable" onclick={() => { handleGoTo(name); driftOpen = false; }}>{name}</button>
                  {/each}
                </div>
              {/if}
              {#if drift.ghostTables.length > 0}
                <div class="drift-section">
                  <h3 class="drift-section-title red">Ghost Tables ({drift.ghostTables.length})</h3>
                  <p class="drift-section-desc">In config but not in database</p>
                  {#each drift.ghostTables as name}<div class="drift-item">{name}</div>{/each}
                </div>
              {/if}
              {#if drift.unmappedFkColumns.length > 0}
                <div class="drift-section">
                  <h3 class="drift-section-title blue">Unmapped FK Columns ({drift.unmappedFkColumns.length})</h3>
                  <p class="drift-section-desc">Look like FKs but have no mapping</p>
                  {#each drift.unmappedFkColumns as { table: tbl, column }}
                    <button class="drift-item clickable" onclick={() => { handleGoTo(tbl); driftOpen = false; }}>
                      <span class="drift-table">{tbl}</span>.<span class="drift-col">{column}</span>
                    </button>
                  {/each}
                </div>
              {/if}
              {#if drift.invalidFkTargets.length > 0}
                <div class="drift-section">
                  <h3 class="drift-section-title red">Invalid FK Targets ({drift.invalidFkTargets.length})</h3>
                  <p class="drift-section-desc">FK mappings to non-existent tables</p>
                  {#each drift.invalidFkTargets as { column, target }}
                    <div class="drift-item">{column} → <span class="drift-col">{target}</span></div>
                  {/each}
                </div>
              {/if}
              {#if drift.constraintDrift?.missingDbConstraints.length}
                {@const cd = drift.constraintDrift}
                <div class="drift-section">
                  <h3 class="drift-section-title amber">Missing DB Constraints ({cd.missingDbConstraints.length})</h3>
                  <p class="drift-section-desc">In config but no actual DB foreign key constraint</p>
                  {#each cd.missingDbConstraints as { table: tbl, column, target }}
                    <button class="drift-item clickable" onclick={() => { handleGoTo(tbl); driftOpen = false; }}>
                      <span class="drift-table">{tbl}</span>.<span class="drift-col">{column}</span> → {target}
                    </button>
                  {/each}
                </div>
              {/if}
              {#if drift.constraintDrift?.extraDbConstraints.length}
                {@const cd = drift.constraintDrift}
                <div class="drift-section">
                  <h3 class="drift-section-title blue">Extra DB Constraints ({cd.extraDbConstraints.length})</h3>
                  <p class="drift-section-desc">DB has FK constraint but no config mapping</p>
                  {#each cd.extraDbConstraints as { table: tbl, column, referencedTable }}
                    <button class="drift-item clickable" onclick={() => { handleGoTo(tbl); driftOpen = false; }}>
                      <span class="drift-table">{tbl}</span>.<span class="drift-col">{column}</span> → {referencedTable}
                    </button>
                  {/each}
                </div>
              {/if}
              {#if drift.constraintDrift?.mismatchedFkTargets.length}
                {@const cd = drift.constraintDrift}
                <div class="drift-section">
                  <h3 class="drift-section-title red">Mismatched FK Targets ({cd.mismatchedFkTargets.length})</h3>
                  <p class="drift-section-desc">Config and DB disagree on FK target table</p>
                  {#each cd.mismatchedFkTargets as { table: tbl, column, configTarget, actualTarget }}
                    <button class="drift-item clickable" onclick={() => { handleGoTo(tbl); driftOpen = false; }}>
                      <span class="drift-table">{tbl}</span>.<span class="drift-col">{column}</span>: config → {configTarget}, DB → {actualTarget}
                    </button>
                  {/each}
                </div>
              {/if}
            </div>
          {/if}
        {/if}

      </div>
    {/if}

  </div>
{/if}

<style>
  :global(.schema-page) {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
</style>
