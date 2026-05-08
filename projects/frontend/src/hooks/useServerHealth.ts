import { useEffect, useRef, useState } from 'react';

export type ServerStatus = 'unknown' | 'up' | 'down';

const POLL_UP_MS = 10_000;
const POLL_DOWN_MS = 2_000;

export function useServerHealth() {
  const [status, setStatus] = useState<ServerStatus>('unknown');
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const check = async () => {
    try {
      const res = await fetch('/api/health', { cache: 'no-store' });
      setStatus(res.ok ? 'up' : 'down');
      timer.current = setTimeout(check, res.ok ? POLL_UP_MS : POLL_DOWN_MS);
    } catch {
      setStatus('down');
      timer.current = setTimeout(check, POLL_DOWN_MS);
    }
  };

  useEffect(() => {
    check();
    return () => { if (timer.current) clearTimeout(timer.current); };
  }, []);

  const retry = () => {
    if (timer.current) clearTimeout(timer.current);
    check();
  };

  return { status, retry };
}
