# One Binary Per Host

> Status: **Living doc.** Why orca ships as one binary per host, and the build sequence.

orca core is one deployable binary per host — its functionality is
augmented by composable, out-of-process plugins. The daemon itself
needs no separate web server process, no node runtime at the target
machine, and no docker requirement.

## How it works

The orca-core binary has no separate web server process, no node
runtime at the target, and no docker requirement for the daemon — but
it is not a closed box: capabilities are composed from out-of-process
plugins. The web UI is served by
the out-of-process **peacock** plugin (repo
[argyle-labs/peacock](https://github.com/argyle-labs/peacock)),
which registers `contract::web` and owns the root route `/`. orca
core serves the UI by proxying unmatched `/` requests to peacock's
`peacock.render` tool. A build with no web plugin registered is
simply headless — the daemon still serves the API, MCP, and mesh.

`rust-embed` is still used for docs and agent assets — markdown and
prompts are baked into the binary as `&'static [u8]` slices and
served from the embedded map without any filesystem dependency on
the target host.

## Build sequence

The release build is driven by `scripts/build-host.sh` and
`scripts/release-lib.sh`:
`cargo build --release` produces the orca binary. The web UI is
built and released independently in the peacock repo (`peacock/ui`
produces the SvelteKit build served by `peacock.render`); the orca
binary builds on its own.

## Dev mode

In dev mode the Rust server binds REST/HTTP on `:12000`. peacock
runs its own Vite dev server, which it declares to orca as the web
provider's `dev_upstream`; orca proxies unmatched `/` requests to
that upstream so the browser gets Vite HMR while `/api/*` is served
by the Rust server directly.

Per-host dev mode runs a peer on HEAD on its own host.

## Self-update

`projects/system/src/update.rs` handles binary replacement
in-process — orca self-updates without sudo. Channels: stable / beta
(prerelease; tags stay `-rc.N`). Dev is a *state* (the `ORCA_DEV` env /
a `-dev+` build), not a channel. `--version <semver>` pins and bypasses the monotonic-newer
veto. Updates fan
out across the pod via mesh-relay — non-networked peers update via
a connected relay.

## Why one binary

Deployment is `cp orca ~/.local/bin/orca` (or the equivalent
service-user path for the system-managed daemon). No Docker, no
node runtime at the install target, no separate web server
process. See [`architecture.md`](architecture.md) for the
three-surface tool model that makes this work.
