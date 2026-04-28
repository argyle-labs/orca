# Single Binary

The brain binary ships alone. No `site/` directory, no npm runtime, no separate install steps at the target machine.

## How it works

`rust-embed` compiles `site/dist/` into the binary at build time. In release mode, every asset — HTML, JS, CSS, source maps — is a `&'static [u8]` slice baked into the executable. At runtime, `brain serve` reads from the embedded map instead of the filesystem.

```rust
#[derive(rust_embed::RustEmbed)]
#[folder = "site/dist/"]
struct Assets;
```

The same pattern applies to `docs/` (this directory): `src/docs.rs` embeds all markdown files so they're accessible via the API without any filesystem dependency.

## Build sequence

`make build` runs these steps in order:

1. `cd site && npm ci && npm run build` — produces `site/dist/`
2. `cargo build --release` — `rust-embed` picks up `site/dist/` and `docs/` during compilation

The `site/dist/` directory must exist before `cargo build --release`. This is why they can't run in parallel.

## Dev mode

In dev mode (`brain serve --dev` or `make dev`):
- The Rust server binds `127.0.0.1:12000` and serves only API routes
- Vite runs separately on `:12001` and proxies `/api/` to `:12000`
- `rust-embed` is still compiled in (debug builds read from disk at runtime, not from compiled bytes)

This means the `site/dist/` directory doesn't need to exist for `cargo build` (debug). It only needs to exist for `cargo build --release`.

## Size tradeoff

The full site bundle (React + Mantine + all dependencies) is approximately 2–3 MB before compression. This is an accepted tradeoff for the single-binary goal. The binary is installed on a developer's machine and run as a local service — distribution size is not a primary concern.

If size becomes critical in the future, large libraries (Mantine, xyflow) could be loaded from a CDN by externalizing them in the Vite build config. This would reduce the embedded bundle to ~200 KB but would require internet access to load the UI.

## Updating the site

```sh
make build    # rebuild site + recompile binary
make install  # build + copy to ~/.local/bin/brain
```

The old binary continues serving the old site until replaced.
