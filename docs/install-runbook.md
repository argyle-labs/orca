# Orca Install Runbook

The operator-facing how-to for putting orca on a fresh host and joining it to a
mesh. Status: Living doc.

Onboarding is **two** steps:

```
   1. install                     2. join the mesh
   ──────────                     ───────────────
   one command on the new host    mDNS offer/accept pairing
                                  from an existing mesh member
```

After step 1 the host runs orca **locally** — the orca-native store is usable
immediately, and the daemon mints its own UUIDv7 identity on first boot. After
step 2 it is a full mesh member. Pairing itself is documented end-to-end in
[`mesh.md`](mesh.md); this runbook covers getting the binary onto the host.

---

## Step 1 — Install

`scripts/install.sh` downloads the release binary for the host's target triple,
sha256-verifies it, and installs it. Run against a release tag:

```sh
sh scripts/install.sh --version vX.Y.Z
```

Run `install.sh` from a checkout (pull mode, needs `GITHUB_TOKEN` because
releases are private) or push it from a controller (see below). The flags,
verified against [`scripts/install.sh`](../scripts/install.sh):

| Flag | Env | Effect |
|---|---|---|
| `--version <tag>` | `ORCA_VERSION` | Install a specific tag (default: latest stable). |
| `--target <triple>` | `ORCA_TARGET` | Override target triple (default: auto-detect). |
| `--dir <path>` | `ORCA_INSTALL_DIR` | Install directory (default: `~/.local/bin`). |
| `--rc`, `--prerelease` | `ORCA_PRERELEASE=1` | Install newest pre-release; pins channel `beta` (tags are `-rc.N`). |
| `--from-file <path>` | `ORCA_FROM_FILE` | Skip the GitHub fetch; install this local binary (push mode). |
| `--skip-sha` | `ORCA_SKIP_SHA=1` | Skip sha256 verification (push mode with pre-verified bytes). |
| `--admin-pubkey <key>` | `ORCA_ADMIN_PUBKEY` | SSH pubkey to install for the orca service user (root mode). |
| `--dev-setup` | `ORCA_DEV_SETUP=1` | Also install the Rust toolchain + cargo-watch for dev mode. |

What install actually does:

1. Detects the target triple (or takes `--target`) and fetches the matching
   release asset from GitHub (unless `--from-file`).
2. Verifies the binary's sha256 against the sibling `.sha256` (unless
   `--skip-sha`).
3. Installs the binary to `~/.local/bin` (or `--dir`).
4. Writes the channel marker to `$ORCA_HOME/channel` (`$ORCA_HOME` defaults to
   `~/.orca`); an `-rc.` tag or `--prerelease` selects `beta`.
5. **When run as root**, re-execs under sudo to target the `orca` service user
   and runs `orca system install --service-user orca` — the binary detects the
   init system (systemd / OpenRC / Unraid) and writes the appropriate
   system-level unit, creates the `orca` user + group with linger, installs the
   `--admin-pubkey` SSH key when provided, creates + chowns the PKI dir, and
   restarts the service. It also symlinks `/usr/local/bin/orca`.

Host identity, mesh certs, and mDNS advertising are the daemon's job on first
boot.

### Push mode (hosts without GitHub reach)

```sh
scripts/deploy-host.sh <host>                       # latest pre-release
scripts/deploy-host.sh --user root --version vX.Y.Z <host>
```

The controller resolves + downloads the asset with `gh`, `scp`s the binary +
its `.sha256` + `install.sh` into `/tmp/` on the target, then runs
`install.sh --from-file /tmp/orca` there. Controller needs `gh`, `scp`, `ssh`,
and `sha256sum`/`shasum`; the target needs only `sh`, `mv`, `chmod`, `mkdir`,
and a sha tool. See [`scripts/deploy-host.sh`](../scripts/deploy-host.sh).

### Service user (admin pubkey)

When install runs as root it creates `orca` and writes the admin pubkey from
`--admin-pubkey` / `ORCA_ADMIN_PUBKEY` to the service user's
`authorized_keys`. Pass the **`.pub` file contents**, never the private key:

```sh
ssh root@host \
  "ORCA_ADMIN_PUBKEY=\"$(cat ~/.ssh/id_ed25519.pub)\" \
   sh -s -- --version vX.Y.Z" \
  < scripts/install.sh
```

Verify the service user came up:

```sh
ssh "orca@$HOST" 'whoami'
ssh "orca@$HOST" '~/.local/bin/orca --version'
ssh "orca@$HOST" '~/.local/bin/orca system detail'   # version, uptime, pending_restart
```

### Privileged helper

The daemon never runs as root. Root work (`storage-apply`, `cron-apply`,
`lxc-exec`, `lxc-push`, `plugin-apply`, `plugin-trust-signed`, `unit-migrate`)
goes through one seam, `utils::privileged`, which runs
`sudo -n /usr/local/libexec/orca/orca admin <op>` with the op as JSON on stdin.
The **sudoers allowlist is the real boundary**: it names that one binary and
those subcommands, nothing else. The checks below only make mistakes fail loud.

- **Linux only.** Privileged ops exist only on Linux. On macOS (and any other
  OS) every privileged op fails with "… is Linux-only"; orca installs no helper
  and no sudoers grant there, and `orca identity privilege repair` reports that
  the drop-in is only managed on Linux.
- **The helper must be installed.** If `/usr/local/libexec/orca/orca` is
  missing, every privileged op fails with an error naming it. orca never falls
  back to the running binary. Fix: `sudo orca identity privilege repair`.
- **The helper must be trusted.** The seam elevates only an absolute path that
  is a real file, not a symlink, and that file and every parent directory must
  be owned by root and not writable by group or other. `repair`, the sudoers
  drift check and the seam all use the same check, so they cannot disagree.
  `sudo` itself comes from a fixed list (`/usr/bin/sudo`, `/bin/sudo`,
  `/usr/local/bin/sudo`), never from `PATH`, and must pass the same check.
- **Unraid** ships `sudo`. Its root filesystem lives in RAM and is rebuilt at
  every boot, so orca keeps the helper and the drop-in on the
  flash drive (`/boot/config/plugins/orca/`) and reinstalls both at boot from a
  marked block in `/boot/config/go`; `repair` rewrites all three.
- **Every call has a timeout, on both sides.** The root helper stops itself at
  its own limit; the caller waits a little longer. If the caller's limit fires,
  it sends SIGTERM (which sudo relays to the helper), waits 10 s, then sends
  SIGKILL, and the error names the op and the limit. Tests enforce that every
  root limit is shorter than its caller limit.

  | Op | Root side | Caller |
  |---|---|---|
  | `storage-apply` | 4 min 30 s | 5 min |
  | `cron-apply` | 40 s | 45 s |
  | `lxc-exec` | 5 min | 5 min 30 s |
  | `lxc-push` | 9 min | 10 min |
  | `plugin-apply` | 30 min | 31 min |
  | `plugin-trust-signed` | 25 s | 30 s |
  | `unit-migrate` | 100 s | 2 min |
  | plugin-loader helper calls | (per op above) | 5 min |

- **Kill switch.** Setting `ORCA_PRIVILEGED_REFUSE` (any value) makes orca
  refuse, without spawning anything:
  - every privileged op above;
  - `pct` (`lxc-exec`, `lxc-push` and its symlink probe, mount diagnostics,
    the toolkit's LXC consumer sweep: list, config, start, reboot, in-guest probe);
  - `docker` (image prune, the runner-wedge `ps`/`kill`, the capability probe,
    the schema view's `docker exec`, the toolkit's consumer sweep: `ps`,
    `inspect`, in-container probe, restart, start, state);
  - `umount` (forced unmount of a stale mount);
  - `chown` (service install log dirs, Unraid appdata, the service user's
    `.ssh`);
  - `launchctl` (macOS agent install and uninstall).

  orca's CI sets it.
- **Test harnesses never touch the host.** If `CARGO_TARGET_TMPDIR`, `NEXTEST`
  or `NEXTEST_RUN_ID` is set, or the running binary is a cargo test binary under
  `target/**/deps/`, everything the kill switch covers is refused and the error
  names the variable or path. The generated systemd units
  (`UnsetEnvironment=`) and the launchd agent (`/usr/bin/env -u …`) clear those
  variables so a daemon never inherits them.
- **`test-scope` never ships.** A binary built with the `orca-utils`
  `test-scope` feature refuses to start outside a test harness, an optimized
  build with it fails to compile, and CI and release run
  `scripts/check-no-test-scope.sh` against every binary crate.

---

## Step 2 — Join the mesh

Pairing is the mDNS **offer/accept** flow. On a shared LAN it is nearly
automatic; the full model, security anchors, and manual fallback live in
[`mesh.md`](mesh.md). The short version:

```sh
orca system list --discovery                 # see candidates + members on the segment
orca system list --pending                  # (on the joiner) shows an incoming offer + its code
orca system join --action accept <6-char-code>     # (on the joiner) completes pairing
```

If mDNS is blocked or the joiner is on a different subnet, push an offer to a
specific address from a secure member with `orca system join --action offer <addr>`, or dial the
inviter from the joiner with `orca system join --action connect <addr>`. A fresh
joiner is **insecure** (holds only its own plugins' secrets) until a secure member promotes it with `orca system mesh tier update --action promote --id <joiner-id>`.

---

## Verify a fully onboarded host

```sh
ssh "orca@$HOST" '~/.local/bin/orca system detail'   # reports the running version
curl -sS "http://$HOST:12000/api/health"             # {"ok":true}
orca system list                                        # the new host appears, healthy
```

The daemon listens on HTTP `:12000`, HTTPS `:12443`, and mesh mTLS `:12002`
(`projects/system/src/daemon.rs`).

---

## Upgrades

The normal path is a mesh self-update, which runs entirely over the mesh:

```sh
system_update(id=<id>, channel=beta)   # apply the channel's latest release
```

See [`force-update-runbook.md`](force-update-runbook.md) for the escalation
ladder when a host is stuck. To re-run install by hand instead:

```sh
ssh "orca@$HOST" "ORCA_ADMIN_PUBKEY=\"$(cat ~/.ssh/id_ed25519.pub)\" \
  sh -s -- --version vX.Y.Z" < scripts/install.sh
# Or push-mode:
scripts/deploy-host.sh --version vX.Y.Z <host>
```

---

## Channel pinning

`install.sh` writes `$ORCA_HOME/channel` (`$ORCA_HOME` defaults to `~/.orca`)
based on the tag shape — an `-rc.` tag lands `beta`. Pass `--rc` / `--prerelease`
to force the beta channel regardless of the resolved tag.

---

## Platform matrix

| Platform | Path | Daemon |
|---|---|---|
| Debian / Ubuntu | pull or push | `systemctl` unit + linger (reference / best-tested) |
| Alpine | pull or push | OpenRC |
| Fedora | pull or push | `systemctl` unit + linger |
| Proxmox host | pull or push, root-flow | `systemctl` unit |
| LXC (unprivileged) | pull or push | user-systemd (UID 0 inside → 100000 on host) |
| Unraid | push only | `/etc/rc.d/rc.orca`, binary persisted to appdata |
| macOS | manual (laptop) | launchd |

The init system is detected and written by `orca system install` — the same
binary handles systemd, OpenRC, and Unraid rc scripts.

---

## Known gotchas

- **`GITHUB_TOKEN` required for pull mode** — releases are private. Use
  `--from-file` (push mode) on hosts without GitHub reach.
- **`--admin-pubkey` when first creating the orca user** — without it the
  controller can't ssh back as `orca`.
- **`PATH` on non-login shells** — invoke `~/.local/bin/orca` by absolute path
  in scripts (install prints a note when `--dir` isn't on `PATH`).
- **Verification is sha256 only** — the signing scheme (cosign vs minisign) is
  still an open decision (see [`planned/`](planned/README.md)).

---

## See also

- [`mesh.md`](mesh.md) — the pairing/trust model in full.
- [`force-update-runbook.md`](force-update-runbook.md) — mesh self-update + the
  force-update escalation ladder.
- [`fleet-wipe-rejoin-runbook.md`](fleet-wipe-rejoin-runbook.md) — coordinated
  identity collapse + re-pair.
- [`planned/`](planned/README.md) — install/lifecycle scope (§1.8 host lifecycle).
