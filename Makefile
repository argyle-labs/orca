.PHONY: build install install-hooks deploy dev run watch clean check release rc promote audit lint format format-check test daemon-install daemon-uninstall kill-dev migrate up down init doctor \
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

# Build frontend + release binary (single self-contained binary with embedded assets).
# OpenAPI→TS codegen is gone — the frontend now talks to orca exclusively through
# the WASM OrcaClient, so there's no `gen.ts` step.
build:
	cargo build --manifest-path projects/server/Cargo.toml
	target/debug/orca spec sync --all || true
	cd projects/frontend && npm ci && npm run build
	cargo build --release --features ui --manifest-path projects/server/Cargo.toml
	@echo "built → target/release/orca"

# Headless build — no embedded web UI. Smaller binary; serves API + MCP only.
build-headless:
	cargo build --release --manifest-path projects/server/Cargo.toml
	@echo "built (headless) → target/release/orca"

# Refresh external rebuy specs without a full build — useful between rebuys
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
	@pkill -f 'orca daemon start' 2>/dev/null || true
	@sleep 1
	@echo "→ dev processes cleared"

# Build release binary and deploy to current system (~/.local/bin/orca)
deploy: build
	@if cmp -s target/release/orca $(INSTALL_PATH); then \
	  echo "binary unchanged — skipping copy and codesign"; \
	else \
	  $(MAKE) kill-dev; \
	  cp target/release/orca $(INSTALL_PATH); \
	  codesign --force --sign - $(INSTALL_PATH); \
	  echo "deployed → $(INSTALL_PATH)"; \
	fi
	$(INSTALL_PATH) daemon install
	@echo "daemon installed"

# Build debug binary and install to ~/.local/bin/brain (dev workflow, no frontend embed)
install-dev:
	cargo build --manifest-path projects/server/Cargo.toml 2>&1 && cp target/debug/orca $(INSTALL_PATH) && echo "installed → $(INSTALL_PATH)"

# Watch for changes and rebuild+install on save (requires cargo-watch)
# Install with: cargo install cargo-watch
watch:
	cargo watch -C projects/server -x 'build' -s 'cp target/debug/orca $(INSTALL_PATH) && echo "→ reloaded"'

# Just check for compile errors without linking
check:
	cargo check --manifest-path projects/server/Cargo.toml

# Dev mode — Rust API :12000 + Vite :12001 + hot reload, secrets injected from 1Password
# Secrets live in the account set by OP_ACCOUNT (.env.local overrides .zshrc default)
dev:
	$(OP_RUN) bash scripts/dev.sh

# Run the installed binary with secrets from 1Password
run:
	$(OP_RUN) $(INSTALL_PATH) serve

# Build and install as a system daemon (launchd on macOS, systemd on Linux)
daemon-install: deploy
	$(INSTALL_PATH) daemon install
	@echo "daemon installed — check status with: orca daemon status"

# Remove daemon service file and stop the service
daemon-uninstall:
	$(INSTALL_PATH) daemon uninstall

# Database migrations
# Usage:
#   make migrate        — apply all pending migrations
#   make migrate up     — apply one migration step up
#   make migrate down   — revert one migration step down
#   make migrate status — show current schema version
#
# $(MAKECMDGOALS) contains all targets from the command line.
# When the user runs "make migrate up", both 'migrate' and 'up' are goals;
# 'migrate' reads UP_OR_DOWN and dispatches accordingly, 'up'/'down' are no-ops.
UP_OR_DOWN := $(filter up down status,$(MAKECMDGOALS))

migrate:
ifeq ($(UP_OR_DOWN),up)
	$(INSTALL_PATH) db up
else ifeq ($(UP_OR_DOWN),down)
	$(INSTALL_PATH) db down
else ifeq ($(UP_OR_DOWN),status)
	$(INSTALL_PATH) db status
else
	$(INSTALL_PATH) db migrate
endif

up down status:
	@: # handled by the migrate target above

clean:
	cargo clean --manifest-path projects/server/Cargo.toml
	rm -rf projects/frontend/dist projects/frontend/node_modules

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
# prettier doesn't speak Rust or Go, so we orchestrate per-language tools.
format:
	@echo "→ rustfmt (workspace)..."
	@cargo fmt --all
	@echo "→ gofmt (sdk-go)..."
	@cd projects/sdk/go && gofmt -l -w .
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
	@echo "→ gofmt -l (sdk-go)..."
	@cd projects/sdk/go && diff=$$(gofmt -l .) && if [ -n "$$diff" ]; then echo "unformatted Go files:"; echo "$$diff"; exit 1; fi
	@echo "→ prettier --check..."
	@cd projects/frontend && npx prettier --check src
	@if command -v taplo >/dev/null 2>&1; then \
	  echo "→ taplo --check..."; \
	  taplo fmt --check; \
	fi

test:
	@echo "→ vitest..."
	@cd projects/frontend && npx vitest run
	@echo "→ cargo test..."
	@cargo test --manifest-path projects/server/Cargo.toml

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
	@echo "→ frontend deps..."
	@cd projects/frontend && npm install
	@echo "→ shopify admin graphql schema (2026-04)..."
	@mkdir -p "$(HOME)/.orca/openapi"
	@npx --yes get-graphql-schema https://shopify.dev/admin-graphql-direct-proxy/2026-04 2>/dev/null \
	  | grep -v "^npm " > "$(HOME)/.orca/openapi/shopify-admin.graphql"
	@echo "  updated → ~/.orca/openapi/shopify-admin.graphql"
	@echo ""
	@echo "ready — run 'make dev' to start, 'make deploy' to build and install"
