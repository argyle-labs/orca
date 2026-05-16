import { defineConfig } from '@hey-api/openapi-ts';

// Generates the typed REST client + zod schemas the SvelteKit UI uses to
// talk to orca's REST API. Source of truth = `orca openapi emit`, which is
// produced from the live axum router + utoipa annotations.
//
// Output is checked into the repo so the build never needs network / cargo.
export default defineConfig({
  input: './openapi.json',
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
