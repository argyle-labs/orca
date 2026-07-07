# Single Binary

The orca binary ships alone. No separate web server process, no
node runtime at the target machine, no docker requirement for the
daemon itself.

## How it works

`rust-embed` compiles `projects/frontend/build/` into the binary
at build time. In release mode every asset — HTML, JS, CSS, source
maps — is a `&'static [u8]` slice baked into the executable. At
runtime, the daemon's HTTP server reads from the embedded map
instead of the filesystem.

The same pattern applies to docs (when needed) — markdown is
served from the embedded map without any filesystem dependency on
the target host.

## Build sequence

The release build is driven by `scripts/build-host.sh` and
`scripts/release-lib.sh` (`project_release_pipeline_arch.md`):

1. Frontend build under `projects/frontend/` produces `build/`.
2. `cargo build --release` picks up `frontend/build/` via
   `rust-embed` and produces the orca binary.

`frontend/build/` must exist before `cargo build --release`. This
is why they don't run in parallel — the Rust step needs the output
of the frontend step.

## Dev mode

In dev mode the Rust server binds REST/HTTP on `:12000` and serves
only API routes. Vite runs separately on its dev port and proxies
`/api/` to `:12000`. The frontend embed is debug-disabled — only
release builds bundle the site.

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
