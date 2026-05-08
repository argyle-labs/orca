import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['src/test-setup.ts'],
    exclude: [
      'node_modules/**',
      'e2e/**',
    ],
  },
  resolve: {
    conditions: ['browser'],
    alias: {
      $lib: '/src/lib',
      '$app/environment': '/src/mocks/app-environment.ts',
    },
  },
});
