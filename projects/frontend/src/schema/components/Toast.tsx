import { useState, useEffect } from 'react';

export function Toast({ timeout = 3000 }: { timeout?: number }) {
  const [visible, setVisible] = useState(true);

  useEffect(() => {
    const t = setTimeout(() => setVisible(false), timeout);
    return () => clearTimeout(t);
  }, [timeout]);

  return (
    <div id="toast" className={visible ? 'show' : ''}>
      Scroll to zoom &middot; Drag to pan &middot; Click for details &middot; Double-click to focus
    </div>
  );
}
