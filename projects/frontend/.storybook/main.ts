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
  viteFinal: async (config) => {
    config.base = '/storybook/';
    return config;
  },
};
export default config;