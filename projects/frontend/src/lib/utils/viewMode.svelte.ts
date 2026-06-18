import { goto } from '$app/navigation';

export type ViewMode = 'tree' | 'table';

// Pure helper: derive view mode from a URL. Tree is the default; only
// `?view=table` flips to the flat table.
export function viewModeFromUrl(url: URL): ViewMode {
  return url.searchParams.get('view') === 'table' ? 'table' : 'tree';
}

// Push a new view mode into the URL via SvelteKit navigation. Replaces
// history state so back/forward isn't polluted; keeps focus + scroll.
export function setViewMode(currentUrl: URL, v: ViewMode) {
  const u = new URL(currentUrl);
  if (v === 'tree') u.searchParams.delete('view');
  else u.searchParams.set('view', v);
  goto(`${u.pathname}${u.search}`, { replaceState: true, keepFocus: true, noScroll: true });
}
