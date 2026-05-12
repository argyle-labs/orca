# Orca Install Runbook

The canonical sequence for bringing a fresh host onto a published orca release.

## Prerequisites

- Local machine has `gh` authenticated against `scottdkey/orca` (private repo).
- Target host reachable over SSH and has `curl` + `sh`.
- A published release tag (e.g. `v0.0.3-rc.11`).

## One-time install (per host)

From your local workstation:

```sh
GH_TOKEN=$(gh auth token)

# 1. Install the binary + channel marker on the remote host.
ssh <host> "GITHUB_TOKEN=$GH_TOKEN sh -s -- --version v0.0.3-rc.11 --prerelease" \
  < scripts/install.sh

# 2. Bootstrap the service. This is idempotent and now also runs `pki ca-init`
#    so plugin-host comes up clean on first boot.
ssh <host> '~/.local/bin/orca daemon install'
```

After step 2, the host has:

- `~/.local/bin/orca` (binary, mode `+x`)
- `~/.orca/channel` (`stable` or `rc`)
- `~/.orca/orca.db` (migrated to latest schema)
- `~/.orca/pki/{ca,server}/...` (CA + server cert)
- systemd user unit (Linux) or LaunchAgent (macOS) — enabled + started
- Daemon on `:12000`, plugin-host on `:12002` (mTLS)

## Verify

```sh
ssh <host> '~/.local/bin/orca daemon status'
ssh <host> 'journalctl --user -u orca -n 20 --no-pager'   # Linux
ssh <host> 'log show --predicate "subsystem == \"com.orca\"" --last 5m'  # macOS
```

Expect to see `listening on 0.0.0.0:12002 (mTLS)` — that's the plugin-host. If
it's missing or you see `server cert not found`, PKI didn't initialize; run
`orca pki ca-init` and `systemctl --user restart orca`.

## Upgrading a host

Same install.sh invocation with the new `--version`. systemd will pick up the
new binary on next restart:

```sh
ssh <host> "GITHUB_TOKEN=$GH_TOKEN sh -s -- --version v0.0.3-rc.12 --prerelease" \
  < scripts/install.sh
ssh <host> 'systemctl --user restart orca'
```

`daemon install` does not need to re-run unless the unit file shape changed.

## Channel pinning

`install.sh` writes `~/.orca/channel` to either `stable` or `rc` based on the
tag shape (anything containing `-rc.` → `rc`). Pass `--prerelease` (or
`ORCA_PRERELEASE=1`) when installing a stable tag but you want the host to
track RCs going forward.

## Known gotchas

- **`GITHUB_TOKEN` is required.** Releases are private. `install.sh` will
  refuse to run without it.
- **PATH on non-login shells.** `~/.local/bin` is usually not on the SSH
  non-login PATH. Always invoke the binary by absolute path (`~/.local/bin/orca`)
  in scripts and runbooks.
- **First-boot plugin-host warning** (pre-rc.12): on hosts installed with
  rc.11 or earlier, `daemon install` did not run `pki ca-init`. The fix lands
  in rc.12; older hosts need a one-time `orca pki ca-init && systemctl --user
  restart orca`.
