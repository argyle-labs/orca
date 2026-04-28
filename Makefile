.PHONY: build install deploy dev watch clean check release audit lint format test

INSTALL_PATH := $(HOME)/.local/bin/brain

# Build frontend + release binary (single self-contained binary with embedded assets)
build:
	cd projects/frontend && npm ci && npm run build
	cargo build --release --manifest-path projects/server/Cargo.toml
	@echo "built → projects/server/target/release/brain"

# Build release binary and deploy to current system (~/.local/bin/brain)
deploy: build
	cp projects/server/target/release/brain $(INSTALL_PATH)
	$(INSTALL_PATH) install-agents
	@echo "deployed → $(INSTALL_PATH)"

# Build debug binary and install to ~/.local/bin/brain (dev workflow, no frontend embed)
install-dev:
	cargo build --manifest-path projects/server/Cargo.toml 2>&1 && cp projects/server/target/debug/brain $(INSTALL_PATH) && echo "installed → $(INSTALL_PATH)"

# Watch for changes and rebuild+install on save (requires cargo-watch)
# Install with: cargo install cargo-watch
watch:
	cargo watch --manifest-path projects/server/Cargo.toml -x 'build' -s 'cp projects/server/target/debug/brain $(INSTALL_PATH) && echo "→ reloaded"'

# Just check for compile errors without linking
check:
	cargo check --manifest-path projects/server/Cargo.toml

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
	 BRAIN_LOG=trace cargo watch -q -c \
	   --manifest-path projects/server/Cargo.toml \
	   -w projects/server/src -w projects/server/Cargo.toml \
	   -x 'run -- serve --dev' 2>&1 | \
	   while IFS= read -r line; do \
	     echo "[server]   $$line"; \
	     echo "$$line" | grep -q "listening on" && \
	       (sleep 0.5 && cd projects/frontend && npm run gen 2>&1 | sed 's/^/[gen]      /') & \
	   done & \
	 (cd projects/frontend && npm run dev 2>&1 | sed 's/^/[frontend] /') & \
	 wait

clean:
	cargo clean --manifest-path projects/server/Cargo.toml
	rm -rf projects/frontend/dist projects/frontend/node_modules

audit:
	@echo "→ npm audit..."
	@cd projects/frontend && npm audit
	@echo "→ cargo audit..."
	@cargo audit --manifest-path projects/server/Cargo.toml

lint:
	@echo "→ eslint..."
	@cd projects/frontend && npx eslint src --ext .ts,.tsx
	@echo "→ clippy..."
	@cargo clippy --manifest-path projects/server/Cargo.toml -- -D warnings

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
	@echo ""
	@echo "ready — run 'make dev' to start, 'make deploy' to build and install"
