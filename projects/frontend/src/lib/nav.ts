/**
 * Sidebar navigation sections.
 *
 * The route paths here are placeholders — most don't exist yet. As pages
 * land in the build queue (see ~/.orca/plans/frontend-build-queue.md),
 * flip `enabled: true` and add the route.
 */

export interface NavItem {
  label: string;
  href: string;
  icon: string; // emoji/glyph for now; swap for SVGs later
  enabled?: boolean;
}

export interface NavSection {
  label: string;
  items: NavItem[];
}

export const NAV_SECTIONS: NavSection[] = [
  {
    label: 'Workspace',
    items: [
      { label: 'Projects', href: '/projects', icon: '◫' },
      { label: 'Plugins', href: '/plugins', icon: '◇' },
      { label: 'Agents', href: '/agents', icon: '✧' },
      { label: 'MCP servers', href: '/mcp', icon: '◊' },
    ],
  },
  {
    label: 'System',
    items: [
      { label: 'Overview', href: '/', icon: '○', enabled: true },
      { label: 'Profile', href: '/profile', icon: '◉' },
      { label: 'Auth', href: '/auth', icon: '⚿' },
      { label: 'Engines', href: '/engines', icon: '◐' },
      { label: 'Update', href: '/system/update', icon: '↻' },
      { label: 'Doctor', href: '/system/doctor', icon: '⚕' },
    ],
  },
  {
    label: 'Infra',
    items: [
      { label: 'Docker', href: '/docker', icon: '⊞' },
      { label: 'Proxmox', href: '/proxmox', icon: '⊟' },
      { label: 'Home Assistant', href: '/ha', icon: '⌂' },
      { label: 'PKI', href: '/pki', icon: '⌽' },
      { label: 'Database', href: '/db', icon: '▤' },
    ],
  },
  {
    label: 'Data',
    items: [
      { label: 'Specs', href: '/specs', icon: '≣' },
      { label: 'Schemas', href: '/schemas', icon: '⌬' },
      { label: 'Docs', href: '/docs', icon: '▭' },
      { label: 'Logs', href: '/logs', icon: '☰' },
    ],
  },
];
