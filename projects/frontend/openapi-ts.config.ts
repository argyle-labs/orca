import { defineConfig } from '@hey-api/openapi-ts';

// Generates the typed REST client + zod schemas the SvelteKit UI uses to talk
// to orca's REST API. Source of truth = the live `/api/openapi` endpoint —
// the dev daemon serves whatever the current axum router + `#[orca_tool]`
// macro emissions produce. No intermediate JSON file gets checked in.
//
// Requires the dev daemon to be running on :12000 when `npm run gen:client`
// fires. If you need to generate offline, run `orca openapi emit > spec.json`
// and pass `--input ./spec.json` on the cli explicitly.
export default defineConfig({
  input: 'http://localhost:12000/api/openapi.json',
  output: {
    path: './src/lib/client',
    postProcess: ['prettier'],
  },
  plugins: [
    '@hey-api/client-fetch',
    '@hey-api/typescript',
    '@hey-api/sdk',
    'zod',
  ],
});
