.PHONY: build install dev watch clean check

INSTALL_PATH := $(HOME)/.local/bin/brain

# Build debug binary and install to ~/.local/bin/brain
install:
	cargo build 2>&1 && cp target/debug/brain $(INSTALL_PATH) && echo "installed → $(INSTALL_PATH)"

# Build release binary and install
release:
	cargo build --release 2>&1 && cp target/release/brain $(INSTALL_PATH) && echo "installed (release) → $(INSTALL_PATH)"

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

# Install cargo-watch if not present
setup:
	cargo install cargo-watch
