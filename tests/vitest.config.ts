import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    projects: [
      {
        test: {
          name: 'structural',
          include: ['src/structural.test.ts'],
          environment: 'node',
          globals: true,
        },
      },
      {
        test: {
          name: 'behavioral',
          include: ['src/behavioral.test.ts'],
          environment: 'node',
          globals: true,
          testTimeout: 60_000,
        },
      },
      {
        test: {
          name: 'local',
          include: ['src/local.test.ts'],
          environment: 'node',
          globals: true,
          // Local models can be slow; generous timeout
          testTimeout: 180_000,
        },
      },
    ],
  },
})
