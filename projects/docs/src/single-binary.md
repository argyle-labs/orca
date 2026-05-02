# Single Binary

The brain binary ships alone. No `projects/frontend/` directory, no npm runtime, no separate install steps at the target machine.

## How it works

`rust-embed` compiles `projects/frontend/dist/` into the binary at build time. In release mode, every asset — HTML, JS, CSS, source maps — is a `&'static [u8]` slice baked into the executable. At runtime, `brain serve` reads from the embedded map instead of the filesystem.

```rust
#[derive(rust_embed::RustEmbed)]
#[folder = "frontend/dist/"]   // relative to projects/server/
struct Assets;
```

The same pattern applies to `docs/` (this directory): `projects/docs/src/lib.rs` embeds all markdown files so they're accessible via the API and MCP tools without any filesystem dependency.

## Build sequence

`make build` runs these steps in order:

1. `cd projects/frontend && npm ci && npm run build` — produces `projects/frontend/dist/`
2. `cargo build --release` — `rust-embed` picks up `frontend/dist/` and the docs during compilation

`frontend/dist/` must exist before `cargo build --release`. This is why they can't run in parallel — the Rust step needs the output of the frontend step.

## Dev mode

In dev mode (`brain serve --dev` or `make dev`):
- The Rust server binds `127.0.0.1:12000` and serves only API routes
- Vite runs separately on `:12001` and proxies `/api/` to `:12000`
- `rust-embed` is still compiled in, but debug builds don't need `frontend/dist/` to exist

This means the frontend doesn't need to be built for `cargo build` (debug). It only needs to exist for `cargo build --release`.

## Size tradeoff

The full site bundle (Svelte 5 + SvelteKit + all dependencies) is smaller than the previous React + Mantine stack — approximately 1–2 MB before compression. This is an accepted tradeoff for the single-binary goal. The binary is installed on a developer's machine and run as a local service — distribution size is not a primary concern.

If size becomes critical in the future, large libraries (`@xyflow/svelte`, Scalar) could be loaded from a CDN by externalizing them in the Vite build config. This would require internet access to load the UI.

## Updating the site

```sh
make build    # rebuild frontend + recompile binary
make install  # build + copy to ~/.local/bin/brain
```

The old binary continues serving the old site until replaced.
