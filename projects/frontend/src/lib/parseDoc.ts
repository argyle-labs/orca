export interface PropertyRow {
  key: string;
  value: string | string[];
}

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
    if (!line.trim()) {
      i++;
      continue;
    }

    const m = /^([^:#][^:]*):\s*(.*)$/.exec(line);
    if (!m) {
      i++;
      continue;
    }
    const key = m[1].trim();
    let val = m[2];

    if (/^[|>][-+]?$/.test(val.trim())) {
      const fold = val.trim().startsWith('>');
      const blockLines: string[] = [];
      i++;
      while (i < lines.length) {
        const next = lines[i];
        if (next.length === 0) {
          blockLines.push('');
          i++;
          continue;
        }
        if (/^\s/.test(next)) {
          blockLines.push(next.replace(/^\s+/, ''));
          i++;
        } else break;
      }
      properties.push({
        key,
        value: fold ? blockLines.join(' ').trim() : blockLines.join('\n').trim(),
      });
      continue;
    }

    if (val.trim().startsWith('[') && val.trim().endsWith(']')) {
      const inner = val.trim().slice(1, -1).trim();
      const items = inner
        ? inner
            .split(',')
            .map(s => stripQuotes(s.trim()))
            .filter(Boolean)
        : [];
      properties.push({ key, value: items });
      i++;
      continue;
    }

    properties.push({ key, value: stripQuotes(val.trim()) });
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
