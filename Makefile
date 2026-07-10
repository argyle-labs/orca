.PHONY: build install install-hooks deploy dev run watch watch-server watch-test watch-wasm clean prune check release rc promote audit lint format format-check test test-changed coverage coverage-html coverage-touched coverage-badge coverage-badge-check cache-stats daemon-install daemon-uninstall kill-dev migrate up down init doctor unraid-install \
  ci release-build release-build-host release-frontend release-sdk-ts release-sdk-kotlin release-checksums release-stage release-publish release-clean

INSTALL_PATH := $(HOME)/.local/bin/orca
ENV_TPL      := .env.orca.tpl

# Local overrides (gitignored) — use OP_ACCOUNT here to select the correct 1P account
-include .env.local
export

# 1Password: CI uses OP_SERVICE_ACCOUNT_TOKEN (set in GitHub Secrets) — op picks it up
# automatically. Local dev: .env.local sets OP_ACCOUNT to the account where brain
# secrets live (automations vault).
OP_RUN := op run --account $(OP_ACCOUNT) --env-file $(ENV_TPL) --

# Verify (and install where possible) the build prerequisites for macOS,
# Arch / CachyOS, and Debian/Ubuntu hosts. CI runners use the workflow's
# setup actions instead — `make doctor` is the read-only check.
init:
	bash scripts/setup.sh

doctor:
	bash scripts/setup.sh --check

# Resolve the host triple once. Both `build` and `deploy` use it so they
# agree on where scripts/build-host.sh wrote the binary.
HOST_TARGET := $(shell rustc -vV | awk '/^host:/ {print $$2}')

# ── Local incremental-build knobs ──────────────────────────────────────────
# Keep native / wasm / mobile cargo target dirs separate so switching
# `--target` doesn't thrash the incremental cache. CI is unaffected — each
# matrix job builds a single target into the default target/.
TARGET_DIR_NATIVE  := target/native
TARGET_DIR_WASM    := target/wasm
TARGET_DIR_IOS     := target/ios
TARGET_DIR_ANDROID := target/android

# Coverage floor — single source of truth, shared with CI (.github/workflows/
# ci.yml: coverage-rust) and the README badge. Ratchet by editing .coverage-floor
# only; never lower. Policy + history: docs/coverage-baseline.md.
COVERAGE_FLOOR := $(shell cat .coverage-floor)

# sccache as the rustc wrapper — shared object cache across surfaces, and
# across hosts if SCCACHE_DIR is pointed at a mesh-synced path.
# Local-only: never set in CI (mozilla-actions/sccache-action would compete
# with Swatinem/rust-cache for the 10 GB GHA cache pool — see CI workflows).
# Only export when the binary actually exists, otherwise `cargo install sccache`
# (in `make install`) can't bootstrap itself.
SCCACHE_BIN := $(shell command -v sccache 2>/dev/null)
ifneq ($(SCCACHE_BIN),)
export RUSTC_WRAPPER ?= sccache
export SCCACHE_DIR   ?= $(HOME)/.cache/sccache
endif

# Build frontend + release binary (single self-contained binary with embedded assets).
# OpenAPI→TS codegen is gone — the frontend now talks to orca exclusively through
# the WASM OrcaClient, so there's no `gen.ts` step. Spec sync is a separate
# `make sync` target — running it here put unrelated network IO on the critical
# path and re-ran on every build.
# Shells out to scripts/build-host.sh so the local build path uses the same
# compile codepath as the release pipeline (scripts/release-lib.sh).
build:
	bash scripts/build-host.sh

# Headless build — no embedded web UI. Smaller binary; serves API + MCP only.
build-headless:
	bash scripts/build-host.sh --headless

# Refresh synced specs from upstream repos. Independent of build.
sync:
	cargo build --manifest-path projects/server/Cargo.toml
	target/debug/orca spec sync --all

# Refresh external specs without a full build — useful between builds
specs:
	cargo build --manifest-path projects/server/Cargo.toml
	target/debug/orca spec sync --all

# Kill all dev processes (cargo-watch, op run dev.sh, orca serve --dev, dev daemon)
kill-dev:
	@echo "→ killing dev processes..."
	@pkill -f 'cargo-watch.*projects/server' 2>/dev/null || true
	@pkill -f 'op run --env-file .env.orca.tpl' 2>/dev/null || true
	@pkill -f 'scripts/dev.sh' 2>/dev/null || true
	@pkill -f 'orca serve --dev' 2>/dev/null || true
	@[ -x $(INSTALL_PATH) ] && $(INSTALL_PATH) system kill-stale 2>/dev/null || true
	@sleep 1
	@echo "→ dev processes cleared"

# Build release binary and deploy to current system (~/.local/bin/orca).
# Install logic (symlink strip, idempotent copy, codesign) lives in
# scripts/release-lib.sh::install_orca_binary so Make and CI runners agree.
deploy:
	@# Pin ORCA_RELEASE_VERSION before invoking build so build.rs bypasses
	@# the dirty/dev fallback (build.rs:32-65). Clean exact-tag deploys get
	@# the tag string; anything else (extra commits, modified tree) gets
	@# `git describe --tags --dirty --always` so the binary still reports a
	@# real identifier instead of a bare CARGO_PKG_VERSION. Either way the
	@# env var is set, so list/detail/ping versions are stable across the
	@# deployed binary lifetime.
	@set -e; \
	if git diff-index --quiet HEAD -- 2>/dev/null && tag=$$(git describe --tags --exact-match HEAD 2>/dev/null); then \
	  ver=$${tag#v}; \
	else \
	  ver=$$(git describe --tags --dirty --always); \
	fi; \
	echo "→ ORCA_RELEASE_VERSION=$$ver"; \
	ORCA_RELEASE_VERSION=$$ver $(MAKE) build
	@$(MAKE) kill-dev
	bash scripts/install-binary.sh target/$(HOST_TARGET)/release/orca $(INSTALL_PATH)
	$(INSTALL_PATH) system install
	@echo "daemon installed"

# Install orca on a private-repo Unraid host via the plugin manager. Cross-
# compiles the linux binary, builds a local-flavored .plg whose binary URL is
# a file:// path on the box, scp's both files, then runs `plugin install`
# remotely. Bootstraps alpha/echo without a public release URL — see
# scripts/unraid-install-plg.sh.
#
# Usage: make unraid-install HOST=alpha [ARCH=x86_64]
HOST ?=
ARCH ?= x86_64
unraid-install:
	@[ -n "$(HOST)" ] || { echo "usage: make unraid-install HOST=<host> [ARCH=x86_64|aarch64]"; exit 2; }
	bash scripts/unraid-install-plg.sh $(HOST) --arch $(ARCH)

# Build debug binary and install to $(INSTALL_PATH). `make dev` runs
# target/debug/orca directly — this target is for the rare case you want the
# system-installed binary to be the debug build.
install-dev:
	cargo build --manifest-path projects/server/Cargo.toml
	bash scripts/install-binary.sh target/debug/orca $(INSTALL_PATH)

# Watch for changes and rebuild+install on save (requires cargo-watch).
# Install cargo-watch with: cargo install cargo-watch
watch: watch-server

watch-server:
	CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) \
	  cargo watch -C projects/server -x 'build' \
	  -s 'bash $(CURDIR)/scripts/install-binary.sh $(CURDIR)/$(TARGET_DIR_NATIVE)/debug/orca $(INSTALL_PATH)'

watch-test:
	CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) \
	  cargo watch -x 'nextest run --workspace --no-fail-fast'

watch-wasm:
	CARGO_TARGET_DIR=$(TARGET_DIR_WASM) \
	  cargo watch -C projects/app-kit -x 'build --target wasm32-unknown-unknown'

# Just check for compile errors without linking
check:
	CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) cargo check --workspace

cache-stats:
	@sccache --show-stats

# Dev mode — Rust API :12000 + Vite :12001 + hot reload, secrets injected from 1Password
# Secrets live in the account set by OP_ACCOUNT (.env.local overrides .zshrc default)
SERVE_BINARY ?=

dev:
	$(OP_RUN) bash scripts/dev.sh $(if $(SERVE_BINARY),--serve-binary,)

# Run the installed binary with secrets from 1Password
run:
	$(OP_RUN) $(INSTALL_PATH) serve

# Build and install as a system daemon (launchd on macOS, systemd on Linux).
# `system install` absorbed the former `system daemon install`.
daemon-install: deploy
	$(INSTALL_PATH) system install
	@echo "daemon installed — check status with: orca system detail"

# Remove daemon service file and stop the service. `system delete` absorbed
# the former `system daemon uninstall`.
daemon-uninstall:
	$(INSTALL_PATH) system delete

# Database migrations
# Usage:
#   make migration                — apply all pending migrations
#   make migration up             — apply one migration step up
#   make migration down           — revert one migration step down
#   make migration status         — show current schema version
#   make migration <slug>         — scaffold up/down files with a UTC timestamp
#
# Migration files live in projects/db/migrations/ and are embedded into the
# binary at compile time via include_dir!. Filenames are
# `<YYYYMMDDHHMMSS>__<slug>.{up,down}.sql`. The timestamp is also the version
# stored in the `schema_migrations` table — never edit a committed file.
#
# Make passes every word on the command line as a separate goal, so for
# `make migration foo` both 'migration' and 'foo' are goals. We catch known
# verbs (up/down/status); anything else after 'migration' is treated as a
# slug and dispatched to the scaffold path.
MIGRATIONS_DIR := projects/db/migrations
MIGRATION_VERBS := up down status
MIGRATION_GOAL := $(filter-out migration,$(MAKECMDGOALS))
MIGRATION_SUB  := $(filter $(MIGRATION_VERBS),$(MIGRATION_GOAL))
MIGRATION_SLUG := $(filter-out $(MIGRATION_VERBS),$(MIGRATION_GOAL))

migration:
ifeq ($(MIGRATION_SUB),up)
	@$(INSTALL_PATH) db up
else ifeq ($(MIGRATION_SUB),down)
	@$(INSTALL_PATH) db down
else ifeq ($(MIGRATION_SUB),status)
	@$(INSTALL_PATH) db status
else ifneq ($(MIGRATION_SLUG),)
	@ts=$$(date -u +"%Y%m%d%H%M%S"); \
	  slug="$(MIGRATION_SLUG)"; \
	  base="$(MIGRATIONS_DIR)/$${ts}__$${slug}"; \
	  mkdir -p "$(MIGRATIONS_DIR)"; \
	  : > "$${base}.up.sql"; \
	  : > "$${base}.down.sql"; \
	  echo "✓ created $${base}.up.sql"; \
	  echo "✓ created $${base}.down.sql"
else
	@$(INSTALL_PATH) db migrate
endif

# No-op stubs so `make migration <verb-or-slug>` doesn't trip on the
# extra goal. The slug stub is gated: without the guard it would absorb
# ANY second goal (e.g. `make deploy` made MIGRATION_SLUG=deploy and
# overrode the real `deploy:` target). The guard ensures the slug stub
# is only created when `migration` is one of the goals.
$(MIGRATION_VERBS):
	@: # handled by the migration target above

ifneq ($(filter migration,$(MAKECMDGOALS)),)
$(MIGRATION_SLUG):
	@: # scaffold handled by the migration target above
endif

clean:
	cargo clean --manifest-path projects/server/Cargo.toml
	rm -rf target/native target/wasm target/ios target/android
	rm -rf projects/frontend/dist projects/frontend/node_modules
	@sccache --zero-stats 2>/dev/null || true

## prune: remove incremental artifacts and stale dep objects without a full clean.
## Safe to run anytime; does not invalidate sccache. Run when target/ grows large.
prune:
	@echo "→ removing incremental artifacts..."
	@find target -type d -name incremental -exec rm -rf {} + 2>/dev/null || true
	@echo "→ removing stale .d dependency files..."
	@find target -name "*.d" -mtime +7 -delete 2>/dev/null || true
	@echo "→ removing orphaned .rmeta files older than 7 days..."
	@find target -name "*.rmeta" -mtime +7 -delete 2>/dev/null || true
	@echo "→ removing debug objects for deps older than 7 days..."
	@find target -path "*/deps/*.o" -mtime +7 -delete 2>/dev/null || true
	@du -sh target/ 2>/dev/null || true

audit:
	@echo "→ npm audit..."
	@cd projects/frontend && npm audit
	@echo "→ cargo audit..."
	@cargo audit --manifest-path projects/server/Cargo.toml

lint:
	@echo "→ prettier check..."
	@cd projects/frontend && npx prettier --check src
	@echo "→ eslint..."
	@cd projects/frontend && npx eslint src --ext .ts,.tsx
	@echo "→ clippy..."
	@cargo clippy --workspace -- -D warnings

# Format every language in the repo. Run via pre-commit hook (see install-hooks)
# and on demand. Each formatter is the canonical one for its language —
# prettier doesn't speak Rust, so we orchestrate per-language tools.
format:
	@echo "→ rustfmt (workspace)..."
	@cargo fmt --all
	@echo "→ prettier (frontend src)..."
	@cd projects/frontend && npx prettier --write src
	@if command -v taplo >/dev/null 2>&1; then \
	  echo "→ taplo (TOML)..."; \
	  taplo fmt; \
	else \
	  echo "→ skipping TOML (install taplo: 'cargo install taplo-cli --locked')"; \
	fi

# Verify formatting without writing — used by CI.
format-check:
	@echo "→ rustfmt --check..."
	@cargo fmt --all -- --check
	@echo "→ prettier --check..."
	@cd projects/frontend && npx prettier --check src
	@if command -v taplo >/dev/null 2>&1; then \
	  echo "→ taplo --check..."; \
	  taplo fmt --check; \
	fi

test:
	@echo "→ vitest..."
	@cd projects/frontend && npx vitest run
	@echo "→ cargo nextest..."
	@CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) \
	  cargo nextest run --workspace --no-fail-fast \
	  || CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) cargo test --workspace
	@echo "→ doctests..."
	@CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) cargo test --workspace --doc --no-fail-fast

# ── Coverage ───────────────────────────────────────────────────────────────
# `coverage` mirrors the CI gate (.github/workflows/ci.yml: coverage-rust).
# Both read the floor from .coverage-floor, so they can never drift.
# Policy + history: docs/coverage-baseline.md.
coverage:
	@CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) \
	  cargo llvm-cov --workspace --no-fail-fast --fail-under-lines $(COVERAGE_FLOOR)

# Human-readable HTML report. Opens under target/native/llvm-cov/html.
coverage-html:
	@CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) \
	  cargo llvm-cov --workspace --no-fail-fast --html
	@echo "→ open $(TARGET_DIR_NATIVE)/llvm-cov/html/index.html"

# Per-file summary filtered to files with uncommitted (or branch-local) Rust
# changes. Use to verify the HARD RULE: any file touched in a slice reaches
# 100% line coverage in the same slice.
coverage-touched:
	@CARGO_TARGET_DIR=$(TARGET_DIR_NATIVE) \
	  cargo llvm-cov --workspace --no-fail-fast --summary-only > target/.cov-summary.txt
	@files=$$(git diff --name-only origin/main...HEAD -- '*.rs'; \
	          git status --porcelain | awk '{print $$2}' | grep -E '\.rs$$'); \
	files=$$(echo "$$files" | sort -u | grep -v '^$$'); \
	if [ -z "$$files" ]; then \
	  echo "no touched .rs files"; \
	else \
	  echo "touched files (line-coverage):"; \
	  for f in $$files; do \
	    short=$$(echo "$$f" | sed -E 's|^projects/||'); \
	    line=$$(grep -E "^$$short[[:space:]]" target/.cov-summary.txt || true); \
	    [ -n "$$line" ] && echo "  $$line" || echo "  $$short  (no coverage row — generated / not in workspace)"; \
	  done; \
	fi

# Regenerate the README coverage badge from .coverage-floor (single source of
# truth). Run after bumping the floor. `coverage-badge-check` fails if the badge
# has drifted from the floor — wire it into CI/pre-push to keep them locked.
COVERAGE_BADGE = [![Coverage](https://img.shields.io/badge/coverage-%E2%89%A5$(COVERAGE_FLOOR)%25%20%E2%86%92%20100%25-blue)](docs/coverage-baseline.md)
coverage-badge:
	@sed -i.bak -E 's|^\[!\[Coverage\].*|$(COVERAGE_BADGE)|' README.md && rm -f README.md.bak
	@echo "README coverage badge set to floor $(COVERAGE_FLOOR)%"

coverage-badge-check:
	@grep -qF 'badge/coverage-%E2%89%A5$(COVERAGE_FLOOR)%25' README.md \
	  || { echo "README coverage badge does not match .coverage-floor ($(COVERAGE_FLOOR)%). Run 'make coverage-badge'."; exit 1; }

# Run tests only for crates (and frontend) whose sources changed vs BASE
# (default `main`) plus anything dirty in the working tree. Falls back to a
# full `make test` if Cargo.toml/Cargo.lock/toolchain/etc tripwires fire.
#   make test-changed                # vs main + dirty tree
#   BASE=origin/main make test-changed
#   make test-changed ARGS=--include-deps
test-changed:
	@TARGET_DIR_NATIVE=$(TARGET_DIR_NATIVE) bash scripts/test-changed.sh $(ARGS)

# Local release pipeline (used when GitHub Actions minutes are exhausted).
# Builds host target only (aarch64-apple-darwin) and pushes to GitHub releases.
# Mirrors .github/workflows/release.yml's RC-then-stable two-step.
#   make release rc BUMP=patch   — cut + publish RC
#   make release promote         — promote latest RC to stable
#
# Same dispatch pattern as `make migrate up/down/status` above:
# RC_OR_PROMOTE picks the action from MAKECMDGOALS; `rc`/`promote` are no-op
# targets that just exist so make doesn't error on the extra goal.
BUMP ?= patch
RC_OR_PROMOTE := $(filter rc promote,$(MAKECMDGOALS))

release:
ifeq ($(RC_OR_PROMOTE),rc)
	bash scripts/release-local.sh rc $(BUMP)
else ifeq ($(RC_OR_PROMOTE),promote)
	bash scripts/release-local.sh promote
else
	@echo "usage: make release rc BUMP=patch|minor|major"; \
	echo "       make release promote"; \
	exit 1
endif

rc promote:
	@: # handled by the release target above

# ── Dev fleet hot-reload ───────────────────────────────────────────────────────
# Pass SERVE_BINARY=1 to also cross-compile for linux x86_64 and serve the
# binary on :12009. Fleet peers auto-update within 10s of each build.
#
#   make dev SERVE_BINARY=1
#
# One-time on each peer: orca update --source http://<mint-ip>:12009

# Delete every published RC release + matching git tag for a given stable.
# Run automatically by `make release promote` already; this target is the
# manual entry point (e.g. cleaning up a botched RC train without promoting).
#
# Usage:
#   make cleanup-rcs VER=0.0.3           # delete v0.0.3-rc.* releases + tags
#   make cleanup-rcs VER=0.0.3 DRY=1     # preview only
cleanup-rcs:
	@if [ -z "$(VER)" ]; then echo "usage: make cleanup-rcs VER=<x.y.z> [DRY=1]"; exit 1; fi
	bash scripts/release-local.sh cleanup-rcs $(VER) $(if $(DRY),--dry-run,)

RUST_VERSION := $(shell cat rust-toolchain.toml | grep channel | sed 's/.*"\(.*\)"/\1/')
NODE_VERSION := $(shell cat .nvmrc | tr -d '[:space:]')

# Point git at the in-repo hooks dir so pre-commit / pre-push are versioned.
install-hooks:
	git config core.hooksPath .githooks
	@chmod +x .githooks/pre-commit .githooks/pre-push 2>/dev/null || true
	@echo "git hooks → .githooks (pre-commit auto-formats, pre-push runs full checks)"

# Install all required tools and dependencies (idempotent — safe to re-run)
install: install-hooks
	@echo "→ rust $(RUST_VERSION)..."
	@command -v rustup >/dev/null 2>&1 || \
	  (echo "  installing rustup..." && \
	   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none && \
	   . $$HOME/.cargo/env)
	@rustup toolchain install $(RUST_VERSION) --no-self-update 2>/dev/null
	@rustup override set $(RUST_VERSION)
	@echo "→ node $(NODE_VERSION)..."
	@if ! command -v nvm >/dev/null 2>&1 && [ ! -f "$$HOME/.nvm/nvm.sh" ]; then \
	  echo "  installing nvm..."; \
	  curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash; \
	fi
	@. $$HOME/.nvm/nvm.sh && nvm install $(NODE_VERSION) --no-progress && nvm use $(NODE_VERSION) && nvm alias default $(NODE_VERSION)
	@echo "→ cargo-watch..."
	@cargo install --list 2>/dev/null | grep -q "^cargo-watch" || cargo install cargo-watch
	@echo "→ cargo-audit..."
	@cargo install --list 2>/dev/null | grep -q "^cargo-audit" || cargo install cargo-audit
	@echo "→ sccache..."
	@cargo install --list 2>/dev/null | grep -q "^sccache" || cargo install sccache --locked
	@echo "→ cargo-nextest..."
	@cargo install --list 2>/dev/null | grep -q "^cargo-nextest" || cargo install cargo-nextest --locked
	@echo "→ frontend deps..."
	@cd projects/frontend && npm install
	@echo "→ shopify admin graphql schema (2026-04)..."
	@mkdir -p "$(HOME)/.orca/openapi"
	@npx --yes get-graphql-schema https://shopify.dev/admin-graphql-direct-proxy/2026-04 2>/dev/null \
	  | grep -v "^npm " > "$(HOME)/.orca/openapi/shopify-admin.graphql"
	@echo "  updated → ~/.orca/openapi/shopify-admin.graphql"
	@echo ""
	@echo "ready — run 'make dev' to start, 'make deploy' to build and install"
