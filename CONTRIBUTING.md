# Contributing to orca

Thanks for working on orca. This page is the short, current version of how a
change lands. For a codebase orientation, start with the
[developer tour](docs/dev/00-tour.md).

## Branch & PR workflow

- **Never commit to `main`.** `main` is protected and only moves through merged
  pull requests. Start every change on a branch:

  ```sh
  git switch -c feat/<short-name>      # or fix/, chore/, docs/, refactor/
  ```

- Push the branch and open a PR **targeting `main`**. CI runs on the PR; a
  green CI run plus review approval is required to merge.
- Keep PRs focused. One logical change per PR makes review (and revert) sane.
- **No AI/Claude attribution** in commit messages, PR titles, or PR bodies.

## Local setup

```sh
make init      # verify/install build prerequisites (rust, node, etc.)
make install   # git hooks + toolchain + cargo tooling
make dev       # hot-reload dev mode (Rust :12000 + Vite :12001)
```

`make dev` / `make run` inject secrets via the 1Password CLI and need
`OP_ACCOUNT` in your environment — see the [README](README.md#development).

## Before you open a PR

Run the gates CI enforces. The pre-commit and pre-push git hooks (installed by
`make install`) run most of these automatically, but run them yourself first to
avoid a red PR:

```sh
make format    # rustfmt + prettier + taplo
make lint      # rustfmt --check + clippy -D warnings + prettier/eslint
make test      # cargo nextest + doctests + vitest
make coverage  # llvm-cov workspace floor (see docs/coverage-baseline.md)
```

For a fast inner loop scoped to changed crates: `./scripts/check-fast.sh` and
`make test-changed`.

## Acceptance criteria

A PR is mergeable when **all** of the following hold. These mirror the CI jobs
in [`.github/workflows/ci.yml`](.github/workflows/ci.yml):

- [ ] **Builds** — Rust workspace and the frontend both build (`build-frontend`).
- [ ] **Formatting** — `cargo fmt --all --check` and `prettier`/`taplo` are clean.
- [ ] **Lint** — `clippy --tests -- -D warnings` passes with zero warnings;
      `eslint` is clean. (No nested `if let` where `collapsible_if` applies —
      use `&&` let-chains.)
- [ ] **Tests pass** — `cargo nextest run --workspace`, `cargo test --doc`, and
      `vitest` (`test-frontend`) are all green.
- [ ] **Coverage** — the workspace line-coverage floor is met or raised, never
      lowered; every `.rs` file the PR touches reaches **100% line coverage**
      (`make coverage-touched`). See [coverage policy](docs/coverage-baseline.md).
- [ ] **Docs updated** — user- or contributor-facing behavior changes ship with
      the matching doc update (README, `docs/`, `CRATE_RESPONSIBILITIES.md`,
      `PLUGINS.md`, as applicable). No dangling references to deleted/renamed files.
- [ ] **No private data** — no secrets, hostnames, private IPs, employer/
      personal data, or AI attribution in the diff.
- [ ] **Scoped** — one logical change; no unrelated drive-by edits.

## Commit messages

Use a conventional prefix (`feat:`, `fix:`, `chore:`, `docs:`, `test:`,
`refactor:`) and an imperative summary. Explain *why* in the body when it isn't
obvious from the diff.

## Releases

Releases are **user-owned**. Do not run `make release`, `make deploy`, or
`gh release create` as part of a contribution, and never push tags. The release
pipeline cuts RC and stable builds; see `.github/workflows/release.yml`.
