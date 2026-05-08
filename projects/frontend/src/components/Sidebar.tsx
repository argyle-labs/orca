import { useEffect, useState } from 'react';
import { Link } from '@tanstack/react-router';
import { Menu, ActionIcon, Group } from '@mantine/core';

function Swatch({ color, size }: { color: string; size: number }) {
  return <span style={{ display: 'inline-block', width: size, height: size, borderRadius: '50%', background: color, flexShrink: 0 }} />;
}
import { SearchModal } from './SearchModal';
import { ServicesPanel } from './ServicesPanel';
import { useAppTheme, THEME_OPTIONS } from '../contexts/ThemeContext';
import type { TreeNode } from '../api/types';
import { useGetTree } from '../api/hooks';

function formatRootLabel(key: string): string {
  return key.replace(/-/g, ' ').replace(/\b\w/g, c => c.toUpperCase());
}

export function Sidebar({ onHealthOpen }: { onHealthOpen: () => void }) {
  const { data: treeData } = useGetTree({}, { refetchOnWindowFocus: false });
  const roots = (treeData ?? {}) as Record<string, TreeNode[]>;

  // Wipe any old dir-open state so stale localStorage can't make dirs appear
  // open and non-collapsible on first load.
  useEffect(() => {
    if (!treeData) return;
    const validRoots = new Set(Object.keys(treeData as object));
    for (let i = localStorage.length - 1; i >= 0; i--) {
      const key = localStorage.key(i);
      if (!key?.startsWith('sidebar-dir-')) continue;
      const rootName = key.split('-')[2];
      if (!validRoots.has(rootName)) localStorage.removeItem(key);
    }
  }, [treeData]);
  const [collapsed, setCollapsed]   = useState(() => localStorage.getItem('sidebar-collapsed') === '1');
  const [searchOpen, setSearchOpen] = useState(false);
  const { theme, mode, setTheme, toggleMode } = useAppTheme();
  const current = THEME_OPTIONS.find((t) => t.id === theme)!;

  // ⌘/ to open search
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === '/') { e.preventDefault(); setSearchOpen(true); }
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, []);

  function toggleCollapse() {
    setCollapsed((c) => {
      const next = !c;
      localStorage.setItem('sidebar-collapsed', next ? '1' : '0');
      return next;
    });
  }

  return (
    <>
      <nav className={`sidebar${collapsed ? ' sidebar-collapsed' : ''}`}>
        {/* Header row */}
        <Group justify="space-between" px="sm" pb="xs" pt={2} wrap="nowrap">
          {!collapsed && <Link to="/" className="site-title" style={{ padding: 0 }}>brain</Link>}

          <Group gap={4} wrap="nowrap" ml={collapsed ? 'auto' : undefined} mr={collapsed ? 'auto' : undefined}>
            {/* Search */}
            <ActionIcon variant="subtle" size="sm" color="gray" title="Search (⌘/)" onClick={() => setSearchOpen(true)}>
              <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8">
                <circle cx="6.5" cy="6.5" r="4.5" /><line x1="10.5" y1="10.5" x2="14" y2="14" />
              </svg>
            </ActionIcon>

            {/* Theme switcher */}
            {!collapsed && (
              <Menu shadow="md" width={140} position="bottom-end">
                <Menu.Target>
                  <ActionIcon variant="subtle" size="sm" title="Switch theme" color="gray">
                    <Swatch color={current.symbol} size={12} />
                  </ActionIcon>
                </Menu.Target>
                <Menu.Dropdown>
                  {THEME_OPTIONS.map((t) => (
                    <Menu.Item key={t.id} leftSection={<Swatch color={t.symbol} size={10} />}
                      onClick={() => setTheme(t.id as import('../contexts/ThemeContext').ThemeName)} fw={theme === t.id ? 600 : 400}>
                      {t.label}
                    </Menu.Item>
                  ))}
                  <Menu.Divider />
                  <Menu.Item onClick={toggleMode}>
                    {mode === 'dark' ? '☀ Light mode' : '☾ Dark mode'}
                  </Menu.Item>
                </Menu.Dropdown>
              </Menu>
            )}

            {/* Collapse toggle */}
            <ActionIcon variant="subtle" size="sm" color="gray" title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'} onClick={toggleCollapse}>
              <svg width="13" height="13" viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth="1.6">
                {collapsed
                  ? <><line x1="2" y1="6" x2="10" y2="6" /><polyline points="6,2 10,6 6,10" /></>
                  : <><line x1="2" y1="6" x2="10" y2="6" /><polyline points="6,2 2,6 6,10" /></>}
              </svg>
            </ActionIcon>
          </Group>
        </Group>

        {!collapsed && (
          <>
            <div className="sidebar-nav">
              <Link to="/schema"    className="sidebar-nav-link" activeProps={{ className: 'sidebar-nav-link active' }}>Schema</Link>
              <Link to="/api-docs"  className="sidebar-nav-link" activeProps={{ className: 'sidebar-nav-link active' }}>API Docs</Link>
              <Link to="/session"   className="sidebar-nav-link" activeProps={{ className: 'sidebar-nav-link active' }}>Session</Link>
              <Link to="/system"    className="sidebar-nav-link" activeProps={{ className: 'sidebar-nav-link active' }}>System</Link>
              <Link to="/docs"      className="sidebar-nav-link" activeProps={{ className: 'sidebar-nav-link active' }}>Resources</Link>
            </div>

            {Object.entries(roots).map(([rootName, tree]) =>
              tree.length === 0 ? null : (
                <RootSection key={rootName} rootName={rootName} tree={tree} />
              )
            )}

            <ServicesPanel onDetailOpen={onHealthOpen} />
          </>
        )}
      </nav>

      <SearchModal open={searchOpen} onClose={() => setSearchOpen(false)} />
    </>
  );
}

function RootSection({ rootName, tree }: { rootName: string; tree: TreeNode[] }) {
  const lsKey = `sidebar-root-${rootName}`;
  const [open, setOpen] = useState(() => localStorage.getItem(lsKey) === '1');

  function toggle() {
    setOpen((o) => {
      const next = !o;
      localStorage.setItem(lsKey, next ? '1' : '0');
      return next;
    });
  }

  return (
    <div className="root-section">
      <button className="root-header" onClick={toggle}>
        <span style={{ fontSize: '0.75rem', opacity: 0.85, marginRight: 4 }}>{open ? '▾' : '▸'}</span>
        {formatRootLabel(rootName)}
      </button>
      {open && (
        <ul className="tree-root">
          {tree.map((node) => (
            <TreeNodeItem key={node.path} node={node} rootName={rootName} depth={0} />
          ))}
        </ul>
      )}
    </div>
  );
}

function TreeNodeItem({ node, rootName, depth }: { node: TreeNode; rootName: string; depth: number }) {
  const lsKey = `sidebar-dir-${rootName}-${node.path}`;
  const [open, setOpen] = useState(() => {
    const saved = localStorage.getItem(lsKey);
    return saved !== null ? saved === '1' : false;
  });

  const paddingLeft = 12 + depth * 14;

  function toggle() {
    setOpen((o) => {
      const next = !o;
      localStorage.setItem(lsKey, next ? '1' : '0');
      return next;
    });
  }

  if (node.type === 'file') {
    const href = '/' + rootName + '/' + node.path.replace(/\.mdx?$/, '').replace(/\\/g, '/');
    return (
      <li>
        <Link
          to={href}
          className="tree-file"
          style={{ paddingLeft, fontSize: '0.75rem', display: 'flex', alignItems: 'center', gap: 6 }}
          activeProps={{ className: 'tree-file active' }}
        >
          {node.order != null && (
            <span style={{ fontSize: '10px', opacity: 0.4, flexShrink: 0, minWidth: 16, textAlign: 'right' }}>
              {String(node.order).padStart(2, '0')}
            </span>
          )}
          {node.name}
        </Link>
      </li>
    );
  }

  return (
    <li>
      <button
        className="tree-dir"
        onClick={toggle}
        style={{
          paddingLeft,
          fontSize: '0.75rem',
          fontWeight: 500,
          color: 'var(--muted)',
          textTransform: 'none',
          letterSpacing: '0',
          display: 'flex',
          alignItems: 'center',
          gap: 6,
        }}
      >
        <span style={{ fontSize: '11px', opacity: 0.9, flexShrink: 0, display: 'inline-block', width: 10 }}>
          {open ? '▼' : '▶'}
        </span>
        {node.order != null && (
          <span style={{ fontSize: '10px', opacity: 0.4, flexShrink: 0, minWidth: 16, textAlign: 'right' }}>
            {String(node.order).padStart(2, '0')}
          </span>
        )}
        {node.name}
      </button>
      {open && node.children && (
        <ul>
          {node.children.map((child) => (
            <TreeNodeItem key={child.path} node={child} rootName={rootName} depth={depth + 1} />
          ))}
        </ul>
      )}
    </li>
  );
}
