/**
 * Sidebar open/closed state.
 *
 * Behavior:
 *   - On load: viewport decides (desktop = open, mobile = closed). No
 *     localStorage persistence — every reload picks the viewport default.
 *   - Crossing the breakpoint mid-session: the sidebar auto-resets to the
 *     new viewport's default (so resizing from desktop → mobile closes it
 *     even if the user had pinned it open in this session).
 *   - User toggle: works within the current viewport class and persists for
 *     the session, but is wiped the moment the viewport class flips.
 *
 * CSS is the source of truth for visibility:
 *   - Default rendering uses a media query on `.sidebar`.
 *   - `[data-sidebar="open"|"closed"]` on <html> overrides that default; this
 *     attribute is the in-session toggle handle.
 *   - On breakpoint crossing we strip the attribute so the media-query
 *     default rules again.
 */

const BP_DESKTOP = 1024;
const ATTR = 'data-sidebar';

function isDesktop(): boolean {
  if (typeof window === 'undefined') return true;
  return window.matchMedia(`(min-width: ${BP_DESKTOP}px)`).matches;
}

function readAttr(): 'open' | 'closed' | null {
  if (typeof document === 'undefined') return null;
  const v = document.documentElement.getAttribute(ATTR);
  return v === 'open' || v === 'closed' ? v : null;
}

let _open = $state<boolean>(true); // overwritten on first browser tick

if (typeof window !== 'undefined') {
  _open = isDesktop();
}

/** Initialise breakpoint listener — call once from the root layout. */
export function initSidebarMediaListener() {
  if (typeof window === 'undefined') return () => {};

  // Re-sync once after hydration (the attribute may already be set if the
  // user toggled within this session and then we re-mounted).
  const attr = readAttr();
  _open = attr ? attr === 'open' : isDesktop();

  const mq = window.matchMedia(`(min-width: ${BP_DESKTOP}px)`);
  const handler = () => {
    // Crossing the breakpoint wipes any session toggle; viewport wins.
    document.documentElement.removeAttribute(ATTR);
    _open = isDesktop();
  };
  mq.addEventListener('change', handler);
  return () => mq.removeEventListener('change', handler);
}

export function getSidebarOpen(): boolean {
  return _open;
}

export function isMobileViewport(): boolean {
  return !isDesktop();
}

export function toggleSidebar() {
  setSessionState(_open ? 'closed' : 'open');
}

export function openSidebar() {
  setSessionState('open');
}

export function closeSidebar() {
  setSessionState('closed');
}

function setSessionState(s: 'open' | 'closed') {
  _open = s === 'open';
  if (typeof document !== 'undefined') {
    document.documentElement.setAttribute(ATTR, s);
  }
}
