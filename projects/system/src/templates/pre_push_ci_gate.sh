#!/usr/bin/env bash
# Global git pre-push gate — materialized by `orca install` to
# ~/.config/git/hooks/pre-push and activated via
# `git config --global core.hooksPath ~/.config/git/hooks`.
#
# WHY THIS EXISTS: setting a global core.hooksPath (for the commit-msg guard)
# makes git ignore every repo's own .git/hooks, which silently disables any
# repo-local pre-push. Without this, nothing runs `cargo fmt --check` / clippy /
# test before a push, so CI becomes the first gate and formatting drift only
# surfaces in the PR. This restores dev/CI parity at the git layer for every
# argyle-labs Rust repo on the machine.
#
# Mirrors CI exactly: `cargo fmt --check` + `cargo clippy --all-targets -D
# warnings` + `cargo test`. Scoped to argyle-labs cargo repos; a no-op for
# everything else (work repos, dotfiles, non-Rust). Chains to a repo's own
# pre-push if it maintains one, so it shadows nothing.
#
# Escape hatches: `git push --no-verify` bypasses entirely; ORCA_PREPUSH_SKIP_TEST=1
# skips only the (slow) test step; ORCA_PREPUSH_SKIP_CLIPPY=1 skips clippy — use
# these when the local orca workspace a plugin patches against is mid-refactor.
set -euo pipefail

run_ci_gate() {
  root="$1"
  cd "$root"

  echo "pre-push: cargo fmt --check"
  if ! cargo fmt --check; then
    echo "pre-push BLOCKED: formatting drift. Run 'cargo fmt' and re-push." >&2
    exit 1
  fi

  if [ -z "${ORCA_PREPUSH_SKIP_CLIPPY:-}" ]; then
    echo "pre-push: cargo clippy --all-targets -- -D warnings"
    if ! cargo clippy --all-targets -- -D warnings; then
      echo "pre-push BLOCKED: clippy warnings. Fix them and re-push" >&2
      echo "  (or ORCA_PREPUSH_SKIP_CLIPPY=1 git push … if the workspace is mid-refactor)." >&2
      exit 1
    fi
  fi

  if [ -z "${ORCA_PREPUSH_SKIP_TEST:-}" ]; then
    echo "pre-push: cargo test"
    if ! cargo test; then
      echo "pre-push BLOCKED: tests failed. Fix them and re-push" >&2
      echo "  (or ORCA_PREPUSH_SKIP_TEST=1 git push … to skip tests)." >&2
      exit 1
    fi
  fi

  echo "pre-push: gate passed."
}

# Only gate argyle-labs cargo repos; no-op elsewhere.
root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -n "$root" ] && [ -f "$root/Cargo.toml" ]; then
  origin="$(git config --get remote.origin.url 2>/dev/null || true)"
  case "$root:$origin" in
    *argyle-labs*) run_ci_gate "$root" ;;
  esac
fi

# Don't shadow a repo-local pre-push the operator maintains: chain to it.
git_dir="$(git rev-parse --absolute-git-dir 2>/dev/null || true)"
local_hook="${git_dir:+$git_dir/hooks/pre-push}"
if [ -n "$local_hook" ] && [ -x "$local_hook" ] && [ "$local_hook" != "$0" ]; then
  exec "$local_hook" "$@"
fi

exit 0
