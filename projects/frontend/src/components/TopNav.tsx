import { useState } from 'react';
import { Link, useLocation } from '@tanstack/react-router';
import { Popover } from '@mantine/core';
import { useAppTheme, THEME_DEFS } from '../contexts/ThemeContext';
import type { ThemeName } from '../contexts/ThemeContext';
import { ServicesPanel } from './ServicesPanel';

// ── Swatch dot ────────────────────────────────────────────────────────────────

function Swatch({ color, size }: { color: string; size: number }) {
  return (
    <span style={{ display: 'inline-block', width: size, height: size, borderRadius: '50%', background: color, flexShrink: 0 }} />
  );
}

// ── Page registry ─────────────────────────────────────────────────────────────

const NAV_PAGES = [
  { to: '/schema',      label: 'Schema',      icon: '⬡' },
  { to: '/api-docs',    label: 'API Docs',     icon: '⚡' },
  { to: '/confluence',  label: 'Confluence',   icon: '📄' },
  { to: '/jira',        label: 'Jira',         icon: '🎯' },
  { to: '/bitbucket',   label: 'Bitbucket',    icon: '⑂' },
  { to: '/resources',   label: 'Resources',    icon: '📚' },
];

function pageLabel(pathname: string): string {
  const match = NAV_PAGES.find((p) => pathname === p.to || pathname.startsWith(p.to + '/'));
  if (match) return match.label;
  if (pathname === '/') return 'Home';
  const seg = pathname.split('/').filter(Boolean)[0];
  return seg ? seg.charAt(0).toUpperCase() + seg.slice(1) : 'Brain';
}

// ── Icons ─────────────────────────────────────────────────────────────────────

const SearchIcon = () => (
  <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8" style={{ flexShrink: 0 }}>
    <circle cx="6.5" cy="6.5" r="4.5" /><line x1="10.5" y1="10.5" x2="14" y2="14" />
  </svg>
);

// ── TopNav ────────────────────────────────────────────────────────────────────

export function TopNav({ onHealthOpen, onSearchOpen }: { onHealthOpen: () => void; onSearchOpen: () => void }) {
  const [navOpen, setNavOpen] = useState(false);
  const [servicesOpen, setServicesOpen] = useState(false);
  const { theme, mode, setTheme, toggleMode } = useAppTheme();
  const location = useLocation();
  const currentLabel = pageLabel(location.pathname);

  return (
    <header className="topnav">
      {/* Left zone */}
      <div className="topnav-left">
        <Popover position="bottom-start" opened={navOpen} onChange={setNavOpen} shadow="md" width="auto">
          <Popover.Target>
            <button className="topnav-popover-btn" onClick={() => setNavOpen((o) => !o)}>
              Navigate <span className="topnav-chevron">{navOpen ? '▴' : '▾'}</span>
            </button>
          </Popover.Target>
          <Popover.Dropdown style={{ padding: 0 }}>
            <div className="topnav-nav-list">
              {NAV_PAGES.map((p) => (
                <Link
                  key={p.to}
                  to={p.to}
                  className="topnav-nav-item"
                  activeProps={{ className: 'topnav-nav-item active' }}
                  onClick={() => setNavOpen(false)}
                >
                  <span className="topnav-nav-icon">{p.icon}</span>
                  {p.label}
                </Link>
              ))}

              {/* Appearance */}
              <div className="topnav-nav-divider" />
              <div className="topnav-nav-section-label">Appearance</div>
              <div className="topnav-nav-appearance-row">
                <button
                  className={`dm-toggle${mode === 'dark' ? ' dm-dark' : ''}`}
                  onClick={toggleMode}
                  title="Toggle light/dark mode"
                >
                  <span className="dm-track">
                    <span className="dm-icon dm-icon-moon">☾</span>
                    <span className="dm-thumb" />
                    <span className="dm-icon dm-icon-sun">☀</span>
                  </span>
                </button>
                {THEME_DEFS.map((t) => (
                  <button
                    key={t.id}
                    className={`topnav-swatch-btn${theme === t.id ? ' active' : ''}`}
                    onClick={() => setTheme(t.id as ThemeName)}
                    title={t.label}
                  >
                    <Swatch color={t.swatch} size={14} />
                  </button>
                ))}
              </div>
            </div>
          </Popover.Dropdown>
        </Popover>

        {/* Mobile: current page breadcrumb */}
        <div className="topnav-breadcrumb">
          <button className="topnav-back-btn" onClick={() => window.history.back()} title="Go back">‹</button>
          <span className="topnav-current-page">{currentLabel}</span>
        </div>
      </div>

      {/* Right zone */}
      <div className="topnav-right">
        <Popover position="bottom-end" width={380} opened={servicesOpen} onChange={setServicesOpen} shadow="md" closeOnClickOutside={false} closeOnEscape={true}>
          <Popover.Target>
            <button className="topnav-popover-btn" onClick={() => setServicesOpen((o) => !o)}>
              Services <span className="topnav-chevron">{servicesOpen ? '▴' : '▾'}</span>
            </button>
          </Popover.Target>
          <Popover.Dropdown style={{ padding: 0 }}>
            <ServicesPanel onDetailOpen={() => { setServicesOpen(false); onHealthOpen(); }} />
          </Popover.Dropdown>
        </Popover>

        <button className="topnav-search" onClick={onSearchOpen}>
          <SearchIcon />
          <span className="topnav-search-text">Search…</span>
          <kbd className="topnav-search-kbd">⌘/</kbd>
        </button>
        <button className="topnav-search-icon-btn" onClick={onSearchOpen} title="Search (⌘/)">
          <SearchIcon />
        </button>
      </div>
    </header>
  );
}
