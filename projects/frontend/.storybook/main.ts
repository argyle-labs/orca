import type { StorybookConfig } from '@storybook/sveltekit';

const config: StorybookConfig = {
  stories: ['../src/**/*.mdx', '../src/**/*.stories.@(js|ts|svelte)'],
  addons: [
    '@storybook/addon-svelte-csf',
    '@chromatic-com/storybook',
    '@storybook/addon-vitest',
    '@storybook/addon-a11y',
    '@storybook/addon-docs',
  ],
  framework: '@storybook/sveltekit',
  // Served behind the orca dev proxy at <baseUrl>/storybook so the browser
  // talks to one origin (matches prod, avoids CORS). The Rust proxy at
  // server/serve/mod.rs forwards /storybook/* to :12002 here.
  //
  // Only rewrite base for the real Storybook dev/build server. The
  // @storybook/addon-vitest plugin reuses this same viteFinal when it builds
  // the Vitest browser-mode server — but that server hosts its orchestrator at
  // `/__vitest_test__/`, so a `/storybook/` base makes every module/asset URL
  // resolve under the wrong prefix, 404s the client bundle, and the browser
  // session never connects back (60s timeout, zero tests run). Vitest sets
  // `process.env.VITEST`, so skip the override there.
  viteFinal: async (config) => {
    if (!process.env.VITEST) {
      config.base = '/storybook/';
    }
    return config;
  },
};
export default config;