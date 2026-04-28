.PHONY: build install dev watch clean check release init audit lint format test

INSTALL_PATH := $(HOME)/.local/bin/brain

# Build site + release binary (single self-contained binary with embedded assets)
build:
	cd site && npm ci && npm run build
	cargo build --release
	@echo "built → target/release/brain"

# Build and install to ~/.local/bin/brain
install: build
	cp target/release/brain $(INSTALL_PATH)
	@echo "installed → $(INSTALL_PATH)"

# Build debug binary and install to ~/.local/bin/brain (dev workflow, no site embed)
install-dev:
	cargo build 2>&1 && cp target/debug/brain $(INSTALL_PATH) && echo "installed → $(INSTALL_PATH)"

# Watch for changes and rebuild+install on save (requires cargo-watch)
# Install with: cargo install cargo-watch
watch:
	cargo watch -x 'build' -s 'cp target/debug/brain $(INSTALL_PATH) && echo "→ reloaded"'

# Just check for compile errors without linking
check:
	cargo check

# Run in dev mode with hot reload — Rust API on :12000, Vite on :12001, gen on each restart
dev:
	@for port in 12000 12001; do \
	  pid=$$(lsof -ti tcp:$$port 2>/dev/null); \
	  if [ -n "$$pid" ]; then \
	    echo "  killing stale process on :$$port (pid $$pid)"; \
	    kill -9 $$pid 2>/dev/null || true; \
	  fi; \
	done
	@echo ""
	@echo "  brain API   →  http://localhost:12000  (cargo-watch)"
	@echo "  brain UI    →  http://localhost:12001  (vite HMR)"
	@echo "  brain gen   →  runs after each backend restart"
	@echo ""
	@trap 'kill 0' SIGINT SIGTERM; \
	 cargo watch -q -c -w src -w Cargo.toml -x 'run -- serve --dev' 2>&1 | \
	   while IFS= read -r line; do \
	     echo "[brain] $$line"; \
	     echo "$$line" | grep -q "listening on" && \
	       (sleep 0.5 && cd site && npm run gen 2>&1 | sed 's/^/[gen]   /') & \
	   done & \
	 (cd site && npm run dev 2>&1 | sed 's/^/[vite]  /') & \
	 wait

clean:
	cargo clean
	rm -rf site/dist site/node_modules

audit:
	@echo "→ npm audit..."
	@cd site && npm audit fix
	@echo "→ cargo audit..."
	@cargo audit

lint:
	@echo "→ eslint..."
	@cd site && npx eslint src --ext .ts,.tsx
	@echo "→ clippy..."
	@cargo clippy -- -D warnings

format:
	@echo "→ prettier..."
	@cd site && npx prettier --write src
	@echo "→ rustfmt..."
	@cargo fmt

test:
	@echo "→ vitest..."
	@cd site && npx vitest run
	@echo "→ cargo test..."
	@cargo test

RUST_VERSION := $(shell cat rust-toolchain.toml | grep channel | sed 's/.*"\(.*\)"/\1/')
NODE_VERSION := $(shell cat .nvmrc | tr -d '[:space:]')

# Install all required tools and dependencies (idempotent — safe to re-run)
init:
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
	@echo "→ site deps..."
	@cd site && npm install
	@echo ""
	@echo "ready — run 'make dev' to start"
