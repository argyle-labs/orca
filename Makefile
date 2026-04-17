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

# Run in dev mode (builds and runs without installing)
dev:
	cargo run -- $(ARGS)

clean:
	cargo clean

# Install cargo-watch if not present
setup:
	cargo install cargo-watch
