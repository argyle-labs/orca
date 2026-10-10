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
# Gates each pushed COMMIT, never HEAD or the working tree: a branch can be
# pushed without checking it out, and a shared checkout may sit on another
# session's branch with its uncommitted files.
#
# Escape hatches: `git push --no-verify` bypasses entirely; ORCA_PREPUSH_SKIP_TEST=1
# skips only the (slow) test step; ORCA_PREPUSH_SKIP_CLIPPY=1 skips clippy — use
# these when the local orca workspace a plugin patches against is mid-refactor;
# ORCA_PREPUSH_SKIP_FRESH=1 skips only the branch-freshness guard.
set -euo pipefail

ZERO=0000000000000000000000000000000000000000
# Pushed refs as "<remote ref> <local sha>" lines; stdin is read once here and
# replayed to a chained repo-local hook.
pushed=""
stdin_buf=""
while read -r lref lsha rref rsha; do
  stdin_buf="$stdin_buf$lref $lsha $rref $rsha
"
  if [ "$lsha" = "$ZERO" ]; then continue; fi # branch deletion
  pushed="$pushed$rref $lsha
"
done

# Refuse to push a commit that does not contain its base branch tip. Uses
# explicit `if` blocks throughout (never `[ … ] && …` chains) so `set -e`
# cannot abort the hook on an expected non-zero test. Best-effort: offline or
# an absent base returns 0 (never block spuriously).
prepush_freshness_guard() {
  if [ -n "${ORCA_PREPUSH_SKIP_FRESH:-}" ]; then return 0; fi
  if [ -z "$pushed" ]; then return 0; fi
  # Resolve the base branch from origin/HEAD; fall back to main.
  base="$(git rev-parse --abbrev-ref origin/HEAD 2>/dev/null | sed 's#^origin/##' || true)"
  if [ -z "$base" ] || [ "$base" = "HEAD" ]; then base=main; fi
  # Offline (or no such remote branch) → don't block.
  if ! git fetch -q origin "$base" 2>/dev/null; then return 0; fi
  if ! git rev-parse -q --verify "refs/remotes/origin/$base" >/dev/null 2>&1; then return 0; fi
  stale=0
  while read -r rref sha; do
    if [ -z "$sha" ]; then continue; fi
    if [ "$rref" = "refs/heads/$base" ]; then continue; fi
    if git merge-base --is-ancestor "refs/remotes/origin/$base" "$sha"; then continue; fi
    behind="$(git rev-list --count "$sha..refs/remotes/origin/$base" 2>/dev/null || echo '?')"
    echo "pre-push BLOCKED: '${rref#refs/heads/}' ($(git rev-parse --short "$sha")) is $behind commit(s) behind origin/$base." >&2
    stale=1
  done <<PUSHED
$pushed
PUSHED
  if [ "$stale" = 1 ]; then
    echo "  A PR from a stale branch is out-of-date-with-base. Rebase before pushing:" >&2
    echo "     git fetch origin $base && git rebase origin/$base" >&2
    echo "  (override once with: ORCA_PREPUSH_SKIP_FRESH=1 git push …)" >&2
    exit 1
  fi
}

# Run a cargo command against an exported tree. cargo-workdir syncs it into a
# fixed per-name dir with a persistent target (incremental, serialized per
# name); without it, build in the export directly.
in_export() {
  if command -v cargo-workdir >/dev/null 2>&1; then
    cargo-workdir --name "$gate_name" "$export_dir" -- "$@"
  else
    (cd "$export_dir" && "$@")
  fi
}

run_ci_gate() {
  sha="$1"
  rm -rf "$export_dir"
  mkdir -p "$export_dir"
  git archive "$sha" | tar -x -C "$export_dir"
  echo "pre-push: gating $(git log -1 --format='%h %s' "$sha")"

  echo "pre-push: cargo fmt --check"
  if ! in_export cargo fmt --check; then
    echo "pre-push BLOCKED: formatting drift. Run 'cargo fmt' and re-push." >&2
    exit 1
  fi

  if [ -z "${ORCA_PREPUSH_SKIP_CLIPPY:-}" ]; then
    echo "pre-push: cargo clippy --all-targets -- -D warnings"
    if ! in_export cargo clippy --all-targets -- -D warnings; then
      echo "pre-push BLOCKED: clippy warnings. Fix them and re-push" >&2
      echo "  (or ORCA_PREPUSH_SKIP_CLIPPY=1 git push … if the workspace is mid-refactor)." >&2
      exit 1
    fi
  fi

  if [ -z "${ORCA_PREPUSH_SKIP_TEST:-}" ]; then
    echo "pre-push: cargo test"
    if ! in_export cargo test; then
      echo "pre-push BLOCKED: tests failed. Fix them and re-push" >&2
      echo "  (or ORCA_PREPUSH_SKIP_TEST=1 git push … to skip tests)." >&2
      exit 1
    fi
  fi

  echo "pre-push: gate passed."
}

# Gate argyle-labs repos; no-op elsewhere. Branch-freshness applies to EVERY
# argyle-labs repo (cargo or not); the fmt/clippy/test CI gate only to commits
# whose tree has a Cargo.toml. Repos with their own core.hooksPath (orca's
# .githooks) never reach this hook, so they are not double-gated.
root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
origin="$(git config --get remote.origin.url 2>/dev/null || true)"
case "$origin" in
  *argyle-labs*)
    prepush_freshness_guard
    gate_name="$(basename "${root:-repo}")-prepush"
    export_dir="$(mktemp -d "${TMPDIR:-/tmp}/prepush-export.XXXXXX")"
    trap 'rm -rf "$export_dir"' EXIT
    gated=" "
    while read -r rref sha; do
      if [ -z "$sha" ]; then continue; fi
      if ! git cat-file -e "$sha:Cargo.toml" 2>/dev/null; then continue; fi
      tree="$(git rev-parse "$sha^{tree}")"
      case "$gated" in *" $tree "*) continue ;; esac
      gated="$gated$tree "
      run_ci_gate "$sha"
    done <<PUSHED
$pushed
PUSHED
    ;;
esac

# Don't shadow a repo-local pre-push the operator maintains: chain to it,
# replaying the ref lines it expects on stdin.
git_dir="$(git rev-parse --absolute-git-dir 2>/dev/null || true)"
local_hook="${git_dir:+$git_dir/hooks/pre-push}"
if [ -n "$local_hook" ] && [ -x "$local_hook" ] && [ "$local_hook" != "$0" ]; then
  printf '%s' "$stdin_buf" | "$local_hook" "$@"
  exit $?
fi

exit 0
