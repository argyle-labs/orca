import { useEffect, useState } from 'react';

export interface Toast {
  id: number;
  type: 'error' | 'success' | 'info';
  message: string;
}

let nextId = 1;

export function toast(type: Toast['type'], message: string) {
  window.dispatchEvent(new CustomEvent('app:toast', { detail: { type, message, id: nextId++ } }));
}

export function Toaster() {
  const [toasts, setToasts] = useState<Toast[]>([]);

  useEffect(() => {
    function handler(e: Event) {
      const t = (e as CustomEvent).detail as Toast;
      setToasts((prev) => [...prev, t]);
      setTimeout(() => setToasts((prev) => prev.filter((x) => x.id !== t.id)), 6000);
    }
    window.addEventListener('app:toast', handler);
    return () => window.removeEventListener('app:toast', handler);
  }, []);

  if (toasts.length === 0) return null;

  return (
    <div className="toaster">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.type}`}>
          <span className="toast-msg">{t.message}</span>
          <button className="toast-close" onClick={() => setToasts((prev) => prev.filter((x) => x.id !== t.id))}>✕</button>
        </div>
      ))}
    </div>
  );
}
