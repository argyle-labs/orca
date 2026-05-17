// Global defaults for the generated hey-api client. Imported once from
// `+layout.svelte` before any SDK call so every fetch from the UI:
//
//  • Sends + accepts the `orca_session` cookie (`credentials: 'include'`).
//    The auth backend is cookie-based for the UI; without this the browser
//    skips the cookie on cross-fetch and every authenticated call 401s.

import { client } from './client/client.gen';

client.setConfig({
  credentials: 'include',
});
