import { useEffect, useState } from 'react';

interface CheckResult { label: string; tool: string; output: string; ok: boolean; }
interface HealthData { timestamp: string; checks: CheckResult[]; }

export function HealthDashboard({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [data, setData] = useState<HealthData | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function runSweep() {
    setLoading(true);
    setError(null);
    try {
      const res = await fetch('/api/rebuy/health/local');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      setData(await res.json());
    } catch (e: any) {
      setError(e.message ?? 'fetch failed');
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    if (open) runSweep();
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [open, onClose]);

  if (!open) return null;

  const ts = data ? new Date(data.timestamp).toLocaleTimeString() : null;

  return (
    <div className="hd-overlay" onClick={onClose}>
      <div className="hd-modal" onClick={(e) => e.stopPropagation()}>
        <div className="hd-header">
          <span className="hd-title">Local Health</span>
          {ts && <span className="hd-ts">Last run: {ts}</span>}
          <button className="hd-refresh" onClick={runSweep} disabled={loading}>
            {loading ? '…' : 'Refresh'}
          </button>
          <button className="hd-close" onClick={onClose}>✕</button>
        </div>
        <div className="hd-body">
          {loading && <div className="hd-loading">Running checks — this may take 10–30 seconds…</div>}
          {!loading && error && <div className="hd-loading" style={{ color: 'var(--color-danger)' }}>{error}</div>}
          {!loading && data && data.checks.map((check) => (
            <CheckCard key={check.tool} check={check} />
          ))}
        </div>
      </div>
    </div>
  );
}

function CheckCard({ check }: { check: CheckResult }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <div className="hd-check">
      <div className="hd-check-header" onClick={() => setExpanded((e) => !e)}>
        <span className={check.ok ? 'hd-dot-ok' : 'hd-dot-fail'}>●</span>
        <span className="hd-check-label">{check.label}</span>
        <span className="hd-check-tool">{check.tool}</span>
        <span className="hd-check-toggle">{expanded ? '▴ collapse' : '▾ expand'}</span>
      </div>
      {expanded && (
        <div className="hd-check-output">
          <pre className="hd-pre">{check.output}</pre>
        </div>
      )}
    </div>
  );
}
