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
# Also guards branch FRESHNESS: refuses to push a feature branch that does not
# contain the tip of its base branch. A stale branch opens/updates a PR that is
# "out-of-date with the base branch", forcing a needless rebase-in-review round
# trip (and, if merged as-is, can reintroduce regressions main already fixed).
# Blocking at push time makes that structurally impossible.
#
# Escape hatches: `git push --no-verify` bypasses entirely; ORCA_PREPUSH_SKIP_TEST=1
# skips only the (slow) test step; ORCA_PREPUSH_SKIP_CLIPPY=1 skips clippy — use
# these when the local orca workspace a plugin patches against is mid-refactor;
# ORCA_PREPUSH_SKIP_FRESH=1 skips only the branch-freshness guard.
set -euo pipefail

# Refuse to push a branch that is behind its base branch. Uses explicit `if`
# blocks throughout (never `[ … ] && …` chains) so `set -e` cannot abort the
# hook on an expected non-zero test — the release-stage footgun. Best-effort:
# offline, detached HEAD, or an absent base all return 0 (never block spuriously).
prepush_freshness_guard() {
  if [ -n "${ORCA_PREPUSH_SKIP_FRESH:-}" ]; then return 0; fi
  cur="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
  if [ -z "$cur" ] || [ "$cur" = "HEAD" ]; then return 0; fi
  # Resolve the base branch from origin/HEAD; fall back to main.
  base="$(git rev-parse --abbrev-ref origin/HEAD 2>/dev/null | sed 's#^origin/##' || true)"
  if [ -z "$base" ]; then base=main; fi
  if [ "$cur" = "$base" ]; then return 0; fi
  # Offline (or no such remote branch) → don't block.
  if ! git fetch -q origin "$base" 2>/dev/null; then return 0; fi
  if ! git rev-parse -q --verify "refs/remotes/origin/$base" >/dev/null 2>&1; then return 0; fi
  # Contains the base tip → fresh, allow.
  if git merge-base --is-ancestor "refs/remotes/origin/$base" HEAD; then return 0; fi
  behind="$(git rev-list --count "HEAD..refs/remotes/origin/$base" 2>/dev/null || echo '?')"
  echo "pre-push BLOCKED: branch '$cur' is $behind commit(s) behind origin/$base." >&2
  echo "  A PR from a stale branch is out-of-date-with-base. Rebase before pushing:" >&2
  echo "     git fetch origin $base && git rebase origin/$base" >&2
  echo "  (override once with: ORCA_PREPUSH_SKIP_FRESH=1 git push …)" >&2
  exit 1
}

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

# Gate argyle-labs repos; no-op elsewhere. Branch-freshness applies to EVERY
# argyle-labs repo (cargo or not); the fmt/clippy/test CI gate only to cargo ones.
root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
origin="$(git config --get remote.origin.url 2>/dev/null || true)"
case "$origin" in
  *argyle-labs*)
    prepush_freshness_guard
    if [ -n "$root" ] && [ -f "$root/Cargo.toml" ]; then
      run_ci_gate "$root"
    fi
    ;;
esac

# Don't shadow a repo-local pre-push the operator maintains: chain to it.
git_dir="$(git rev-parse --absolute-git-dir 2>/dev/null || true)"
local_hook="${git_dir:+$git_dir/hooks/pre-push}"
if [ -n "$local_hook" ] && [ -x "$local_hook" ] && [ "$local_hook" != "$0" ]; then
  exec "$local_hook" "$@"
fi

exit 0
