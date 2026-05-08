// Renders YAML frontmatter as an Obsidian-style properties panel above the
// document body. Mirrors the visual model from Obsidian: each property is
// one row with a small icon, the key, and the value (string, list, or
// multi-line string).

interface PropertyRow {
  key: string;
  value: string | string[];
}

const ICON = (
  <svg
    aria-hidden="true"
    width="14"
    height="14"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth="2"
    strokeLinecap="round"
    strokeLinejoin="round"
  >
    <line x1="4" y1="6"  x2="20" y2="6"  />
    <line x1="4" y1="12" x2="20" y2="12" />
    <line x1="4" y1="18" x2="14" y2="18" />
  </svg>
);

export function PropertiesPanel({ rows }: { rows: PropertyRow[] }) {
  if (!rows.length) return null;
  return (
    <section className="properties-panel" aria-label="Properties">
      <h2 className="properties-panel-title">Properties</h2>
      <div className="properties-panel-grid">
        {rows.map((row) => (
          <div key={row.key} className="properties-panel-row">
            <div className="properties-panel-key">
              <span className="properties-panel-icon">{ICON}</span>
              <span className="properties-panel-key-text">{row.key}</span>
            </div>
            <div className="properties-panel-value">
              {Array.isArray(row.value) ? (
                <div className="properties-panel-tags">
                  {row.value.map((v, i) => (
                    <span key={i} className="properties-panel-tag">{v}</span>
                  ))}
                </div>
              ) : (
                <span>{row.value}</span>
              )}
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}

// ── Frontmatter parser ────────────────────────────────────────────────────────
// A focused subset of YAML that covers what brain's docs actually use:
//   key: scalar
//   key: [a, b, c]
//   key: |-
//     multi-line block string
//   key: >-
//     folded block string
//
// More exotic YAML (anchors, nested maps, complex flow lists) falls back to a
// raw-string value so the property still surfaces — it just doesn't try to
// pretty-print structures it can't safely round-trip.

export interface ParsedDoc {
  properties: PropertyRow[];
  body: string;
}

export function parseDoc(raw: string): ParsedDoc {
  const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(raw);
  if (!match) return { properties: [], body: raw };

  const fm = match[1];
  const body = raw.slice(match[0].length);
  const lines = fm.split(/\r?\n/);

  const properties: PropertyRow[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }

    const m = /^([^:#][^:]*):\s*(.*)$/.exec(line);
    if (!m) { i++; continue; }
    const key = m[1].trim();
    let raw = m[2];

    // Block scalar (|, |-, >, >-) — gather indented lines that follow
    if (/^[|>][-+]?$/.test(raw.trim())) {
      const fold = raw.trim().startsWith('>');
      const blockLines: string[] = [];
      i++;
      while (i < lines.length) {
        const next = lines[i];
        if (next.length === 0) { blockLines.push(''); i++; continue; }
        if (/^\s/.test(next)) {
          blockLines.push(next.replace(/^\s+/, ''));
          i++;
        } else break;
      }
      const value = fold ? blockLines.join(' ').trim() : blockLines.join('\n').trim();
      properties.push({ key, value });
      continue;
    }

    // Inline list: [a, b, c]
    if (raw.trim().startsWith('[') && raw.trim().endsWith(']')) {
      const inner = raw.trim().slice(1, -1).trim();
      const items = inner
        ? inner.split(',').map((s) => stripQuotes(s.trim())).filter(Boolean)
        : [];
      properties.push({ key, value: items });
      i++;
      continue;
    }

    // Scalar (strip surrounding quotes if present)
    properties.push({ key, value: stripQuotes(raw.trim()) });
    i++;
  }

  return { properties, body };
}

function stripQuotes(s: string): string {
  if ((s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'"))) {
    return s.slice(1, -1);
  }
  return s;
}
