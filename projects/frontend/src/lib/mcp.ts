export async function callTool(
  server: string,
  name: string,
  args: Record<string, unknown> = {},
): Promise<string> {
  const res = await fetch('/api/mcp/run', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ server, name, arguments: args }),
  });
  const data = await res.json();
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  return data.content?.[0]?.text ?? JSON.stringify(data);
}

export function stripAnsi(s: string): string {
  return s.replace(/(\x1b|\x1B)\[[\d;]*m|\[[\d;]+m/g, '');
}

export function parseStatus(output: string): boolean | null {
  if (!output) return null;
  const lower = stripAnsi(output).toLowerCase();
  if (/\b(stopped|not running|not found|not connected|not started|down|failed|error)\b/.test(lower))
    return false;
  if (output.includes('"success": false')) return false;
  if (/\b(running|up|healthy|connected|active|started)\b/.test(lower)) return true;
  if (output.includes('"success": true')) return true;
  return null;
}

export function parseBadge(id: string, output: string): string | null {
  const clean = stripAnsi(output);
  if (id === 'engines') {
    const m = clean.match(/cluster:\s*(\w+)/i);
    return m ? m[1] : null;
  }
  if (id === 'env') {
    const m = clean.match(/Active Database Profile:[\s\S]{0,120}?(local|stage|prod(?:-primary)?)/i);
    return m ? m[1] : null;
  }
  if (id === 'tunnel') {
    const m = clean.match(/cluster:\s*(\w+)/i) ?? clean.match(/profile:\s*(\w[\w-]*)/i);
    return m ? m[1] : null;
  }
  return null;
}

export type BadgeTier = 'danger' | 'warn' | 'dim';

export function badgeTier(badge: string): BadgeTier {
  if (/^prod/i.test(badge)) return 'danger';
  if (/^stag/i.test(badge)) return 'warn';
  return 'dim';
}
