<!--
  Keep PRs focused: one logical change. See CONTRIBUTING.md for the full workflow.
  No AI/Claude attribution in the title or body.
-->

## Summary

<!-- What does this PR do, and why? -->

## Changes

<!-- Bullet the notable changes. -->
-

## Testing

<!-- How did you verify this? Paste the commands you ran. -->
- [ ] `make lint`
- [ ] `make test`
- [ ] `make coverage`

## Acceptance criteria

<!-- Mirrors .github/workflows/ci.yml and CONTRIBUTING.md. All must hold to merge. -->
- [ ] Branched off `main` (not committing to `main` directly)
- [ ] Rust + frontend build
- [ ] `cargo fmt --check` and prettier/taplo clean
- [ ] `clippy --tests -- -D warnings` clean (no `collapsible_if` violations)
- [ ] `cargo nextest`, doctests, and `vitest` pass
- [ ] Coverage floor met or raised (never lowered); touched `.rs` files at 100% line coverage (`make coverage-touched`)
- [ ] Docs updated for any behavior change; no dangling references
- [ ] No secrets, private hostnames/IPs, or AI attribution in the diff
- [ ] One focused change; no unrelated edits
