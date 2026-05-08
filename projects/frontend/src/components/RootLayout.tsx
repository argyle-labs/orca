import { useEffect, useState } from 'react';
import { Outlet, useLocation } from '@tanstack/react-router';
import { TopNav } from './TopNav';
import { CommandPalette } from './CommandPalette';
import { HealthDashboard } from './HealthDashboard';
import { SearchModal } from './SearchModal';
import { useNavHistory } from '../hooks/useNavHistory';
import { useServerHealth } from '../hooks/useServerHealth';

export function RootLayout() {
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [searchOpen, setSearchOpen]   = useState(false);
  const [healthOpen, setHealthOpen]   = useState(false);
  const location = useLocation();
  const { record } = useNavHistory();

  const openPalette = () => { setSearchOpen(false); setPaletteOpen(true); };
  const openSearch  = () => { setPaletteOpen(false); setSearchOpen(true); };
  const { status: serverStatus, retry: retryServer } = useServerHealth();

  useEffect(() => {
    const path = location.pathname;
    const title = path === '/' ? 'Home'
      : path.replace(/^\//, '').replace(/\//g, ' / ').replace(/-/g, ' ');
    record(path, title);
  }, [location.pathname]);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey) {
        if (e.key === 'k') { e.preventDefault(); openPalette(); }
        if (e.key === '/') { e.preventDefault(); openSearch(); }
      }
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, []);

  return (
    <div className="layout">
      <TopNav onHealthOpen={() => setHealthOpen(true)} onSearchOpen={openSearch} />
      {serverStatus === 'down' && (
        <div style={{
          position: 'fixed', bottom: 16, left: '50%', transform: 'translateX(-50%)',
          background: 'var(--bg)', border: '1px solid var(--color-danger)', borderRadius: 8,
          padding: '10px 16px', display: 'flex', alignItems: 'center', gap: 12,
          zIndex: 9999, fontSize: 13, color: 'var(--color-danger)', boxShadow: '0 4px 16px rgba(0,0,0,0.4)',
        }}>
          <span>⚠ backend unreachable</span>
          <button
            onClick={retryServer}
            style={{
              background: 'var(--color-danger)', color: 'var(--bg)', border: 'none', borderRadius: 4,
              padding: '3px 10px', cursor: 'pointer', fontSize: 12, fontWeight: 600,
            }}
          >
            retry
          </button>
        </div>
      )}
      <main className="content">
        <Outlet />
      </main>
      <SearchModal open={searchOpen} onClose={() => setSearchOpen(false)} />
      <CommandPalette open={paletteOpen} onClose={() => setPaletteOpen(false)} />
      <HealthDashboard open={healthOpen} onClose={() => setHealthOpen(false)} />
    </div>
  );
}
