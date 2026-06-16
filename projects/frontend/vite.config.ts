import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [sveltekit()],
  server: {
    port: 12001,
    host: '127.0.0.1',
    // HMR goes through the orca proxy on :12000 (which forwards WSS → 12001)
    // so the browser only ever talks to one origin. This makes session
    // cookies same-origin and avoids cross-port ETP cookie blocks.
    hmr: { clientPort: 12000, protocol: 'ws' },
    // Pre-transform critical first-paint modules on dev server boot so the
    // browser doesn't pay per-module transform latency on a cold reload.
    // Keep this list TIGHT — every entry blocks dev server start.
    warmup: {
      clientFiles: [
        './src/routes/+layout.svelte',
        './src/routes/+layout.ts',
        './src/routes/+page.svelte',
        './src/routes/+page.ts',
        './src/lib/stores/runTool.ts',
        './src/lib/stores/session.svelte.ts',
        './src/lib/stores/theme.svelte.ts',
      ],
    },
  },
  optimizeDeps: {
    // Pre-bundle these as one cached chunk so the browser fetches them once
    // (cold dev start) rather than as N un-bundled module requests on every
    // route change. hey-api's runtime client + Svelte's reactivity helpers
    // are stable across our edits and benefit from a single pre-bundled blob.
    include: ['@hey-api/client-fetch'],
    // sdk.gen.ts is intentionally NOT pre-bundled — it's dynamically
    // imported via runTool.ts so it lands in its own chunk and stays off
    // the first-paint critical path.
    exclude: ['$lib/client/sdk.gen'],
  },
  build: {
    // The graphiql+react bundle (~1850 kB) and codemirror (~960 kB) are lazy-loaded
    // per-route via dynamic import — they don't affect initial page load.
    chunkSizeWarningLimit: 2000,
    rolldownOptions: {
      output: {
        // Rolldown's `codeSplitting.strategy: 'smart'` isn't in vite's
        // RollupOptions types yet (vite still ships rollup typings). Cast
        // through `unknown` rather than `any` so the escape is explicit
        // and contained to this single line.
        codeSplitting: { strategy: 'smart' } as unknown as boolean,
      },
    },
  },
});
