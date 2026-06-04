// Global defaults for the generated hey-api client. Imported once from
// `+layout.svelte` before any SDK call so every fetch from the UI:
//
//  • Sends + accepts the `orca_session` cookie (`credentials: 'include'`).
//    The auth backend is cookie-based for the UI; without this the browser
//    skips the cookie on cross-fetch and every authenticated call 401s.
//  • Uses an empty baseUrl so every request inherits the page's origin.
//    The generated client embeds `http://localhost:12000`, which makes API
//    calls cross-site whenever the user reaches the daemon by IP, mDNS name,
//    alias, or HTTPS port — and Firefox then drops Set-Cookie because
//    SameSite=Lax forbids cross-site cookie storage.

import { client } from './client/client.gen';

client.setConfig({
  baseUrl: '',
  credentials: 'include',
});
