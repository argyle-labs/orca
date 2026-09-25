# Test coverage baseline & policy

Orca's goal is **100% line coverage** of the Rust workspace, reached by a
**ratchet**: the CI floor only ever goes up. A PR may never lower it.

## The two rules

1. **Workspace floor (gate).** CI runs `cargo llvm-cov --workspace
   --fail-under-lines <floor>`. If coverage drops below the floor, the
   `test-rust` job fails and the PR is blocked. The floor is raised every
   time new tests land — never lowered.
2. **Touched-files rule (aim, enforced by review).** Any `.rs` file you add or
   modify in a change should reach **100% line coverage in the same change**.
   Use `make coverage-touched` to check only the files your branch touched.

The workspace floor is a *trailing* number (where the whole codebase is today);
the touched-files rule is the *leading* edge that drags it toward 100%.

## Current floor

The floor lives in **one file**: `.coverage-floor` (repo root, a bare integer).
Every consumer reads from that single source, so they all stay in sync:

| Consumer | How it reads the floor |
|----------|------------------------|
| CI gate (`.github/workflows/ci.yml` → `test-rust`) | `--fail-under-lines "$(cat .coverage-floor)"` — authoritative; blocks pushes below it |
| `make coverage` (local) | `COVERAGE_FLOOR := $(shell cat .coverage-floor)` — mirrors the CI gate exactly |
| README badge | regenerated from the floor by `make coverage-badge`; `make coverage-badge-check` fails on drift |

> To raise the floor: edit `.coverage-floor` only, then run `make
> coverage-badge` to refresh the README badge, and add a line to the history
> table below. Never lower it.

The live floor is whatever `.coverage-floor` holds — always read that file,
never trust a number quoted in prose (including here).

### Floor history

History used to live in a comment above the `coverage-rust` job in `ci.yml`.
That job is gone — coverage, linting and testing now share one build in
`test-rust` — so the history lives here instead, where it cannot be deleted
along with a job.

| date | floor | note |
|------|-------|------|
| 2026-05-19 | 44.98% | initial baseline |
| 2026-05-20 | 47.52% | |
| 2026-05-21 | 51.24% | |
| 2026-09-25 | 72 | floor as of the move to a single shared build |

Measured workspace line coverage on 2026-09-25 was **84.98%**, i.e. ~13 points
of headroom above the floor. Ratcheting is a deliberate policy decision, not
something to do automatically because the number drifted up.

## Running coverage locally

```sh
make coverage          # the gate: llvm-cov --workspace --fail-under-lines $(cat .coverage-floor)
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

## Scope

The ratchet covers this repository's Rust workspace. The web UI ships as the
external [`peacock`](https://github.com/argyle-labs/peacock) plugin and carries
its own test suite in that repo.
