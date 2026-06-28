# Test coverage baseline & policy

Orca's goal is **100% line coverage** of the Rust workspace, reached by a
**ratchet**: the CI floor only ever goes up. A PR may never lower it.

## The two rules

1. **Workspace floor (gate).** CI runs `cargo llvm-cov --workspace
   --fail-under-lines <floor>`. If coverage drops below the floor, the
   `coverage-rust` job fails and the PR is blocked. The floor is raised every
   time new tests land — never lowered.
2. **Touched-files rule (aim, enforced by review).** Any `.rs` file you add or
   modify in a change should reach **100% line coverage in the same change**.
   Use `make coverage-touched` to check only the files your branch touched.

The workspace floor is a *trailing* number (where the whole codebase is today);
the touched-files rule is the *leading* edge that drags it toward 100%.

## Current floor

| Where | Value | Notes |
|-------|-------|-------|
| CI gate (`.github/workflows/ci.yml` → `coverage-rust`) | **51%** | authoritative; blocks PRs below it |
| `make coverage` (local) | **51%** | mirrors the CI gate; keep in sync |

> When you raise the floor, bump **both** the CI workflow and the `make
> coverage` target in `Makefile` in the same PR, and note the jump (date +
> what added the coverage) in the CI workflow comment.

History of the floor lives in the comment block above the `coverage-rust` job
in `ci.yml` (e.g. 2026-05-19 baseline 44.98% → 47.52% → 51.24%).

## Running coverage locally

```sh
make coverage          # the gate: llvm-cov --workspace --fail-under-lines 51
make coverage-html     # human-readable HTML report (opens under target/native/llvm-cov/html)
make coverage-touched  # per-file line coverage, filtered to .rs files this branch changed
```

`make coverage-touched` is the fastest way to confirm the touched-files rule
before opening a PR.

## What counts

- **Unit + integration tests** run via `cargo nextest run --workspace`.
- **Doctests** run via `cargo test --doc --workspace`.
- Generated code (e.g. spec-derived clients) and code with no coverage row are
  reported as such by `coverage-touched` and are out of scope for the
  touched-files rule.

## Frontend

The frontend test suite (`vitest`) runs in CI (`test-frontend`) via `npm run
test:run` and must pass, but it is not part of the Rust line-coverage ratchet.
