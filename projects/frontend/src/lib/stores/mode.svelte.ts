import { browser } from '$app/environment';

const KEY = 'orca-section';

// Section is now a free string — "orca" is the built-in default,
// everything else comes from plugin mode declarations.
let _section = $state<string>(browser ? (localStorage.getItem(KEY) ?? 'orca') : 'orca');

export function getSection(): string { return _section; }

export function setSection(s: string): void {
  _section = s;
  if (browser) localStorage.setItem(KEY, s);
}

// Core orca nav links — always available in the "orca" section.
// Plugin nav_links are fetched separately and merged in by the Sidebar.
export const ORCA_CORE_NAV: ({ href: string; label: string } | { panel: string })[] = [
  { panel: 'specs' },
];

// Doc vault roots that are always shown in "orca" section.
export const ORCA_DOC_ROOTS = ['brain', 'dotfiles'];
