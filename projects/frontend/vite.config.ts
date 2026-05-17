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
  },
  build: {
    // The graphiql+react bundle (~1850 kB) and codemirror (~960 kB) are lazy-loaded
    // per-route via dynamic import — they don't affect initial page load.
    chunkSizeWarningLimit: 2000,
    rolldownOptions: {
      output: {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        codeSplitting: { strategy: 'smart' } as any,
      },
    },
  },
});
