.PHONY: build install deploy dev run watch clean check release audit lint format test daemon-install daemon-uninstall kill-dev

INSTALL_PATH := $(HOME)/.local/bin/brain
ENV_TPL      := .env.brain.tpl

# 1Password: CI uses OP_SERVICE_ACCOUNT_TOKEN (set in GitHub Secrets) — op picks it up
# automatically. Local dev requires OP_ACCOUNT set in dotfiles/.zshrc.
# Token lives in: 1Password → automations → brain → ci_service_account_token
OP_RUN := op run --env-file $(ENV_TPL) --

# Build frontend + release binary (single self-contained binary with embedded assets)
build:
	cargo build --manifest-path projects/server/Cargo.toml
	target/debug/brain spec dump > /tmp/brain-openapi.json
	target/debug/brain spec sync --all || true
	cd projects/frontend && npm ci && npx tsx scripts/gen.ts --file /tmp/brain-openapi.json && npm run build
	cargo build --release --manifest-path projects/server/Cargo.toml
	@echo "built → target/release/brain"

# Refresh external rebuy specs without a full build — useful between rebuys
specs:
	cargo build --manifest-path projects/server/Cargo.toml
	target/debug/brain spec sync --all

# Kill all dev processes (cargo-watch, op run dev.sh, brain serve --dev, dev daemon)
kill-dev:
	@echo "→ killing dev processes..."
	@pkill -f 'cargo-watch.*projects/server' 2>/dev/null || true
	@pkill -f 'op run --env-file .env.brain.tpl' 2>/dev/null || true
	@pkill -f 'scripts/dev.sh' 2>/dev/null || true
	@pkill -f 'brain serve --dev' 2>/dev/null || true
	@pkill -f 'brain daemon start' 2>/dev/null || true
	@sleep 1
	@echo "→ dev processes cleared"

# Build release binary and deploy to current system (~/.local/bin/brain)
deploy: kill-dev build
	cp target/release/brain $(INSTALL_PATH)
	codesign --force --sign - $(INSTALL_PATH)
	$(INSTALL_PATH) daemon install
	@echo "deployed → $(INSTALL_PATH)"

# Build debug binary and install to ~/.local/bin/brain (dev workflow, no frontend embed)
install-dev:
	cargo build --manifest-path projects/server/Cargo.toml 2>&1 && cp target/debug/brain $(INSTALL_PATH) && echo "installed → $(INSTALL_PATH)"

# Watch for changes and rebuild+install on save (requires cargo-watch)
# Install with: cargo install cargo-watch
watch:
	cargo watch -C projects/server -x 'build' -s 'cp target/debug/brain $(INSTALL_PATH) && echo "→ reloaded"'

# Just check for compile errors without linking
check:
	cargo check --manifest-path projects/server/Cargo.toml

# Dev mode — Rust API :12000 + Vite :12001 + hot reload, secrets injected from 1Password
# Requires: OP_ACCOUNT set in dotfiles/.zshrc (see README)
dev:
	op signin --account $(OP_ACCOUNT)
	$(OP_RUN) bash scripts/dev.sh

# Run the installed binary with secrets from 1Password
run:
	$(OP_RUN) $(INSTALL_PATH) serve

# Build and install as a system daemon (launchd on macOS, systemd on Linux)
daemon-install: deploy
	$(INSTALL_PATH) daemon install
	@echo "daemon installed — check status with: brain daemon status"

# Remove daemon service file and stop the service
daemon-uninstall:
	$(INSTALL_PATH) daemon uninstall

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

format:
	@echo "→ prettier..."
	@cd projects/frontend && npx prettier --write src
	@echo "→ rustfmt..."
	@cargo fmt --manifest-path projects/server/Cargo.toml

test:
	@echo "→ vitest..."
	@cd projects/frontend && npx vitest run
	@echo "→ cargo test..."
	@cargo test --manifest-path projects/server/Cargo.toml

RUST_VERSION := $(shell cat rust-toolchain.toml | grep channel | sed 's/.*"\(.*\)"/\1/')
NODE_VERSION := $(shell cat .nvmrc | tr -d '[:space:]')

# Install all required tools and dependencies (idempotent — safe to re-run)
install:
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
	@mkdir -p "$(HOME)/brain/openapi"
	@npx --yes get-graphql-schema https://shopify.dev/admin-graphql-direct-proxy/2026-04 2>/dev/null \
	  | grep -v "^npm " > "$(HOME)/brain/openapi/shopify-admin.graphql"
	@echo "  updated → ~/brain/openapi/shopify-admin.graphql"
	@echo ""
	@echo "ready — run 'make dev' to start, 'make deploy' to build and install"
