/**
 * Sidebar navigation sections.
 *
 * Only built pages appear here. Add new entries when their route lands.
 */

export interface NavItem {
  label: string;
  href: string;
  icon: string;
  enabled?: boolean;
}

export interface NavSection {
  label: string;
  items: NavItem[];
}

export const NAV_SECTIONS: NavSection[] = [
  {
    label: 'System',
    items: [{ label: 'Systems', href: '/', icon: '○', enabled: true }],
  },
  {
    label: 'Infra',
    items: [{ label: 'Docker', href: '/docker', icon: '⊞', enabled: true }],
  },
];
