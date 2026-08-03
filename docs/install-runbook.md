# Orca Install Runbook

The operator-facing how-to for putting orca on a fresh host and joining it to a
pod. Status: Living doc.

Onboarding is **two** steps:

```
   1. install                     2. join the pod
   ──────────                     ───────────────
   one command on the new host    mDNS offer/accept pairing
                                  from an existing pod member
```

After step 1 the host runs orca **locally** — the orca-native store is usable
immediately, and the daemon mints its own UUIDv7 identity on first boot. After
step 2 it is a full pod member. Pairing itself is documented end-to-end in
[`pod.md`](pod.md); this runbook covers getting the binary onto the host.

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

---

## Step 2 — Join the pod

Pairing is the mDNS **offer/accept** flow. On a shared LAN it is nearly
automatic; the full model, security anchors, and manual fallback live in
[`pod.md`](pod.md). The short version:

```sh
orca pod discover                 # see candidates + members on the segment
orca pod pending                  # (on the joiner) shows an incoming offer + its code
orca pod accept <6-char-code>     # (on the joiner) completes pairing
```

If mDNS is blocked or the joiner is on a different subnet, push an offer to a
specific address from a secure member with `orca pod offer <addr>`, or dial the
inviter from the joiner with `orca pod join <addr>`. Secrets storage on a fresh
joiner stays **off** until the operator opts in with `orca pod self-secure on`.

---

## Verify a fully onboarded host

```sh
ssh "orca@$HOST" '~/.local/bin/orca system detail'   # reports the running version
curl -sS "http://$HOST:12000/api/health"             # {"ok":true}
orca pod list                                        # the new host appears, healthy
```

The daemon listens on HTTP `:12000`, HTTPS `:12443`, and mesh mTLS `:12002`
(`projects/system/src/daemon.rs`).

---

## Upgrades

The normal path is a mesh self-update, which runs entirely over the pod mesh:

```sh
system_update(peer=<id>, channel=beta)   # apply the channel's latest release
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
  still an open decision (see [`ROADMAP.md`](ROADMAP.md)).

---

## See also

- [`pod.md`](pod.md) — the pairing/trust model in full.
- [`force-update-runbook.md`](force-update-runbook.md) — mesh self-update + the
  force-update escalation ladder.
- [`fleet-wipe-rejoin-runbook.md`](fleet-wipe-rejoin-runbook.md) — coordinated
  identity collapse + re-pair.
- [`ROADMAP.md`](ROADMAP.md) — install/lifecycle scope (§1.8 host lifecycle).
