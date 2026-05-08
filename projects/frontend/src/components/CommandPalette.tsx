import { useEffect, useRef, useState } from 'react';

interface McpTool {
  server: string;
  name: string;
  description: string;
  inputSchema: {
    type?: string;
    properties?: Record<string, { type: string; description?: string; enum?: string[] }>;
    required?: string[];
  };
}

function formatToolName(tool: McpTool): string {
  // Strip server-derived prefix then capitalize: "rebuy_env_start" → "Env: Start"
  const prefix = tool.server.replace(/-/g, '_') + '_';
  const stripped = tool.name.startsWith(prefix) ? tool.name.slice(prefix.length) : tool.name;
  const parts = stripped.split('_');
  if (parts.length === 1) return parts[0].charAt(0).toUpperCase() + parts[0].slice(1);
  const [group, ...rest] = parts;
  const label = rest.map((p) => p.charAt(0).toUpperCase() + p.slice(1)).join(' ');
  return `${group.charAt(0).toUpperCase() + group.slice(1)}: ${label}`;
}

function groupTools(tools: McpTool[]): Record<string, McpTool[]> {
  const groups: Record<string, McpTool[]> = {};
  for (const tool of tools) {
    if (!groups[tool.server]) groups[tool.server] = [];
    groups[tool.server].push(tool);
  }
  return groups;
}

function ToolList({
  tools,
  query,
  onQuery,
  onSelect,
}: {
  tools: McpTool[];
  query: string;
  onQuery: (q: string) => void;
  onSelect: (t: McpTool) => void;
}) {
  const inputRef = useRef<HTMLInputElement>(null);
  useEffect(() => { inputRef.current?.focus(); }, []);

  const lower = query.toLowerCase();
  const filtered = query
    ? tools.filter((t) => t.name.toLowerCase().includes(lower) || t.description.toLowerCase().includes(lower))
    : tools;
  const groups = groupTools(filtered);
  const groupOrder = Object.keys(groups).sort((a, b) => a.localeCompare(b));

  return (
    <>
      <div className="cp-header">
        <input
          ref={inputRef}
          className="cp-search"
          placeholder="Search MCP tools…"
          value={query}
          onChange={(e) => onQuery(e.target.value)}
        />
      </div>
      <div className="cp-body">
        {groupOrder.map((group) => (
          <div key={group}>
            <div className="cp-group-label">{group}</div>
            {groups[group].map((tool) => (
              <div key={`${tool.server}/${tool.name}`} className="cp-tool-row" onClick={() => onSelect(tool)}>
                <div style={{ display: 'flex', alignItems: 'baseline', gap: '0.5rem' }}>
                  <span className="cp-tool-name">{formatToolName(tool)}</span>
                  <span style={{ fontSize: '0.7rem', fontFamily: 'monospace', color: 'var(--muted)', opacity: 0.7 }}>{tool.name}</span>
                </div>
                <span className="cp-tool-desc">{tool.description}</span>
              </div>
            ))}
          </div>
        ))}
        {filtered.length === 0 && (
          <div style={{ padding: '1rem', color: 'var(--muted)', fontSize: '0.82rem' }}>No commands match.</div>
        )}
      </div>
    </>
  );
}

function ToolDetail({
  tool,
  form,
  onForm,
  onBack,
  onRun,
  running,
  output,
  error,
}: {
  tool: McpTool;
  form: Record<string, string | boolean>;
  onForm: (key: string, val: string | boolean) => void;
  onBack: () => void;
  onRun: () => void;
  running: boolean;
  output: string | null;
  error: string | null;
}) {
  const properties = tool.inputSchema.properties ?? {};
  const required = new Set(tool.inputSchema.required ?? []);

  return (
    <>
      <div className="cp-header">
        <button className="cp-back" onClick={onBack}>← back</button>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 1 }}>
          <span style={{ fontSize: '0.88rem', fontWeight: 600, color: 'var(--text)' }}>{formatToolName(tool)}</span>
          <span style={{ fontSize: '0.72rem', fontFamily: 'monospace', color: 'var(--muted)' }}>{tool.server} / {tool.name}</span>
        </div>
      </div>
      <div className="cp-body">
        <div className="cp-detail">
          <div className="cp-detail-desc">{tool.description}</div>

          {Object.entries(properties).map(([key, prop]) => {
            const isRequired = required.has(key);
            if (prop.type === 'boolean') {
              return (
                <label className="cp-checkbox-row" key={key}>
                  <input
                    type="checkbox"
                    checked={!!(form[key])}
                    onChange={(e) => onForm(key, e.target.checked)}
                  />
                  {key}{isRequired ? ' *' : ''}{prop.description ? ` — ${prop.description}` : ''}
                </label>
              );
            }
            if (prop.enum && prop.enum.length > 0) {
              return (
                <div className="cp-field" key={key}>
                  <label className="cp-label">
                    {key}{isRequired ? ' *' : ''}{prop.description ? ` — ${prop.description}` : ''}
                  </label>
                  <select
                    className="cp-select"
                    value={(form[key] as string) ?? ''}
                    onChange={(e) => onForm(key, e.target.value)}
                  >
                    <option value="">— choose —</option>
                    {prop.enum.map((v) => <option key={v} value={v}>{v}</option>)}
                  </select>
                </div>
              );
            }
            return (
              <div className="cp-field" key={key}>
                <label className="cp-label">
                  {key}{isRequired ? ' *' : ''}{prop.description ? ` — ${prop.description}` : ''}
                </label>
                <input
                  className="cp-input"
                  type={prop.type === 'number' || prop.type === 'integer' ? 'number' : 'text'}
                  value={(form[key] as string) ?? ''}
                  onChange={(e) => onForm(key, e.target.value)}
                />
              </div>
            );
          })}

          <button className="cp-run" onClick={onRun} disabled={running}>
            {running ? 'Running…' : 'Run'}
          </button>

          {error && <div className="cp-error">{error}</div>}
          {output !== null && <pre className="cp-output">{output}</pre>}
        </div>
      </div>
    </>
  );
}

export function CommandPalette({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [query, setQuery] = useState('');
  const [tools, setTools] = useState<McpTool[]>([]);
  const [selected, setSelected] = useState<McpTool | null>(null);
  const [form, setForm] = useState<Record<string, string | boolean>>({});
  const [output, setOutput] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open && tools.length === 0) {
      fetch('/api/mcp/tools').then((r) => r.json()).then(setTools).catch(() => {});
    }
  }, [open, tools.length]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        if (selected) { setSelected(null); }
        else { onClose(); }
      }
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [open, selected, onClose]);

  if (!open) return null;

  function handleSelect(tool: McpTool) {
    setSelected(tool);
    setForm({});
    setOutput(null);
    setError(null);
  }

  function handleBack() {
    setSelected(null);
    setOutput(null);
    setError(null);
  }

  function handleFormField(key: string, val: string | boolean) {
    setForm((prev) => ({ ...prev, [key]: val }));
  }

  async function handleRun() {
    if (!selected) return;
    setRunning(true);
    setOutput(null);
    setError(null);
    try {
      const res = await fetch('/api/mcp/run', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ server: selected.server, name: selected.name, arguments: form }),
      });
      const data = await res.json();
      if (!res.ok) { setError(data.error ?? 'Request failed'); return; }
      setOutput(data.content?.[0]?.text ?? JSON.stringify(data));
    } catch (err) {
      setError(String(err));
    } finally {
      setRunning(false);
    }
  }

  return (
    <div className="cp-overlay" onClick={onClose}>
      <div className="cp-modal" onClick={(e) => e.stopPropagation()}>
        {selected ? (
          <ToolDetail
            tool={selected}
            form={form}
            onForm={handleFormField}
            onBack={handleBack}
            onRun={handleRun}
            running={running}
            output={output}
            error={error}
          />
        ) : (
          <ToolList tools={tools} query={query} onQuery={setQuery} onSelect={handleSelect} />
        )}
      </div>
    </div>
  );
}
