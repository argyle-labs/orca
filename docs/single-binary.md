# Single Binary

The orca binary ships alone. No separate web server process, no
node runtime at the target machine, no docker requirement for the
daemon itself.

## How it works

The orca binary is self-contained: it has no separate web server
process, no node runtime at the target, and no docker requirement.
The web UI is **not** embedded in the orca binary — it is served by
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
`scripts/release-lib.sh` (`project_release_pipeline_arch.md`):
`cargo build --release` produces the orca binary. The web UI is
built and released independently in the peacock repo (`peacock/ui`
produces the SvelteKit build served by `peacock.render`); it is no
longer a prerequisite of the orca binary build.

## Dev mode

In dev mode the Rust server binds REST/HTTP on `:12000`. peacock
runs its own Vite dev server, which it declares to orca as the web
provider's `dev_upstream`; orca proxies unmatched `/` requests to
that upstream so the browser gets Vite HMR while `/api/*` is served
by the Rust server directly.

For per-host dev mode (peer running HEAD on its own host),
see `project_dev_mode_toolchain_bootstrap.md` and `project_dev_channel_plan.md`.

## Self-update

`projects/system/src/update.rs` handles binary replacement
in-process — orca self-updates without sudo
(`feedback_orca_self_updates_no_sudo.md`). Channels: stable / rc /
dev. `--version <semver>` pins and bypasses the monotonic-newer
veto (`feedback_dev_mode_does_not_block_updates.md`). Updates fan
out across the pod via mesh-relay — non-networked peers update via
a connected relay (`project_update_paths_first_class.md`,
`project_must_update_our_systems.md`).

## Why one binary

Deployment is `cp orca ~/.local/bin/orca` (or the equivalent
service-user path for the system-managed daemon). No Docker, no
node runtime at the install target, no separate web server
process. See [`architecture.md`](architecture.md) for the
four-surface tool model that makes this work.
