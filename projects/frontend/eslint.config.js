import js from '@eslint/js';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    rules: {
      // App-wide rule: no `any` types. Use `unknown` and narrow at the boundary.
      // Rust analog: clippy's disallowed_types on serde_json::Value + JsonAny
      // inside projects/tools-def/ (see projects/tools-def/clippy.toml).
      '@typescript-eslint/no-explicit-any': 'error',
      // NOTE: @typescript-eslint/no-unsafe-assignment and no-unsafe-member-access
      // require type-aware linting (parserOptions.project), which this config
      // does not enable — turning them on without that setup is a no-op. If/when
      // we add typed linting, flip them on here.
    },
  },
  {
    // Auto-generated files: suppress unused-vars (generated code imports types for re-export)
    files: ['src/api/hooks.ts', 'src/api/types.ts'],
    rules: {
      '@typescript-eslint/no-unused-vars': 'off',
    },
  },
  {
    ignores: ['dist/**', 'node_modules/**', 'coverage/**'],
  },
);
