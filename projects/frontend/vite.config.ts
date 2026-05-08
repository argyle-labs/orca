import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [sveltekit()],
  server: {
    port: 12001,
    host: '127.0.0.1',
    hmr: { clientPort: 12001 },
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
