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
  throwOnError: true,
});

// Stamp every outbound request with a fresh `x-correlation-id`. The server
// echoes it back on the response and threads it through tool dispatch +
// pod/exec mesh calls, so a single browser action traces end-to-end across
// every host involved. `crypto.randomUUID()` is v4, good enough for tracing
// (the server uses uuidv7 when synthesizing; both round-trip as strings).
client.interceptors.request.use(request => {
  if (!request.headers.has('x-correlation-id')) {
    request.headers.set('x-correlation-id', crypto.randomUUID());
  }
  return request;
});
