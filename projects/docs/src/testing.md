# Testing

## Running tests

```sh
make test          # run all tests (vitest + cargo test)
cargo test         # Rust tests only
cd projects/frontend && npx vitest run   # frontend tests only (vitest)
cd projects/frontend && npx playwright test  # e2e tests (playwright)
```

## Rust tests

### Unit tests (inline)

Unit tests live alongside the code they test in `#[cfg(test)]` blocks at the bottom of each file.

| File | What's tested |
|------|--------------|
| `src/serve/tree.rs` | Tree building, title extraction, compact logic, file collection |
| `src/auth.rs` | Keychain storage round-trip |
| `src/config.rs` | Config loading and path resolution |
| `src/ledger.rs` | Token counting arithmetic |
| `src/serve/api.rs` | Handler-level response validation |
| `src/tools/fs.rs` | File read/write/edit operations |

Run a specific test module:
```sh
cargo test serve::tree
cargo test ledger
```

Run a specific test by name:
```sh
cargo test build_tree_raw_returns_md_files
```

### Integration tests

`tests/tools_test.rs` tests the full tool operation pipeline using real filesystem operations in temp directories. These are integration tests — they create actual files, run actual operations, and assert on results.

Tests covered:
- `test_read_file` — file read
- `test_write_and_read` — nested directory write + read
- `test_edit_file` — exact string replacement
- `test_glob_pattern` — `*.rs` pattern matching
- `test_grep_content` — line matching with needle
- `test_strip_frontmatter` — YAML frontmatter removal
- `test_model_parse` — model string prefix detection

### Test dependencies

`tempfile = "3"` is the only test dependency. It provides `tempfile::tempdir()` which creates a scoped temp directory that's automatically cleaned up when the handle is dropped.

```toml
[dev-dependencies]
tempfile = "3"
```

### Writing new Rust tests

For pure logic, add a `#[cfg(test)] mod tests { ... }` block at the bottom of the file. For anything touching the filesystem, use `tempfile::tempdir()`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_function_does_the_thing() {
        let tmp = tempfile::tempdir().unwrap();
        // ... use tmp.path() for all file operations
        // tmp is cleaned up automatically on drop
    }
}
```

For async tests (axum handlers, backend calls), use `#[tokio::test]`:

```rust
#[tokio::test]
async fn my_async_test() {
    // ...
}
```

## Frontend tests

### Vitest (unit / component)

`make test` runs `npx vitest run` in `projects/frontend/`. Test files are `*.test.ts` co-located with the source they test. `@testing-library/svelte` is installed.

Example Svelte component test (matches the existing `Button.test.ts` pattern):

```typescript
// projects/frontend/src/lib/components/MyComponent.test.ts
import { render } from '@testing-library/svelte';
import MyComponent from './MyComponent.svelte';

test('renders label', () => {
  const { getByText } = render(MyComponent, { props: { label: 'Click me' } });
  expect(getByText('Click me')).toBeTruthy();
});
```

Run with `svelte-check` first to catch template type errors that Vitest alone won't catch:

```sh
cd projects/frontend && npm run check && npm run test:run
```

Priority components for coverage:
1. `Sidebar.svelte` — tree loading, collapse state, localStorage persistence
2. `SearchModal.svelte` — keyboard interaction, query debounce
3. `DataTable.svelte` — sort/filter logic

### Playwright (e2e)

`package.json` includes `test:e2e` scripts. E2e tests would live in `projects/frontend/e2e/` and test full user flows against a running `brain serve` instance. Not yet written.

## What is NOT tested

- **Backend API calls** — LM Studio and Claude responses are not mocked. Tests that require a live LLM are impractical in CI; keep them manual.
- **MCP server protocol** — The JSON-RPC dispatch in `mcp.rs` is not tested. Priority for future coverage.
- **Docker integration** — `docker_services_handler` and `docker_action_handler` require a running Docker daemon.
- **Keychain** — `auth.rs` keychain tests are marked `#[ignore]` on non-macOS and in CI because they require a real keychain.

## CI considerations

`cargo test` runs cleanly with no external dependencies except `tempfile`. Before adding tests that require Docker, LM Studio, or the macOS Keychain, gate them behind `#[cfg_attr(not(feature = "integration"), ignore)]` or a separate test binary.
