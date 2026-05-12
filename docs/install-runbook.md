# Orca Install Runbook

How to bring a fresh host onto a published orca release. Two paths:

- **Pull (primary)** — host fetches its own binary from GitHub. One ssh.
- **Push (fallback)** — controller ships bytes over ssh. Use when the host
  has no `curl`/`wget` (`baldur`), no GitHub reach (`freyr` behind VPN), or
  any other minimal-image case.

Both paths use the same `scripts/install.sh` on the target and result in the
same end state.

---

## The `orca` service user

Whenever install.sh runs as root, it creates a least-privileged `orca` user
and installs for that user. Root never owns the binary.

| | |
|---|---|
| Home | `/var/lib/orca` |
| Shell | `/bin/bash` (needed for `systemctl --user`) |
| Groups | `docker`, `systemd-journal` (best-effort — skipped if absent) |
| Sudo | **never** |
| Linger | enabled via `loginctl enable-linger` so user-systemd persists |
| `authorized_keys` | populated **only** from `--admin-pubkey`. Root's keys are not copied. |

If you need to grant orca additional capabilities later (host reboot, raw
socket binding, etc.), do it explicitly via polkit rules or capabilities —
not by adding it to `sudo`.

---

## Getting your admin pubkey

`--admin-pubkey` / `ORCA_ADMIN_PUBKEY` is the SSH public key that will be
written to `/var/lib/orca/.ssh/authorized_keys` so the controller can `ssh
orca@host` afterward. It is the **`.pub` file**, never the private key.

```sh
# Preferred: ed25519
cat ~/.ssh/id_ed25519.pub

# Older keys, in priority order:
cat ~/.ssh/id_ecdsa.pub
cat ~/.ssh/id_rsa.pub
```

No key yet? Generate one — orca will not use the private side:

```sh
ssh-keygen -t ed25519 -C "orca-admin@$(hostname)"
cat ~/.ssh/id_ed25519.pub
```

Pass it via either form. **Both must be the literal one-line pubkey** —
quoted on the command line, expanded by your local shell before the value
travels to the host:

```sh
# flag
... install.sh --admin-pubkey "$(cat ~/.ssh/id_ed25519.pub)"

# env
ORCA_ADMIN_PUBKEY="$(cat ~/.ssh/id_ed25519.pub)" sh install.sh
```

Verify after install:

```sh
ssh "$HOST" 'sudo cat /var/lib/orca/.ssh/authorized_keys'
ssh "orca@$HOST" 'whoami'   # → orca
```

---

## Path 1 — Pull install (one ssh)

For hosts with `curl` or `wget` and GitHub reach.

```sh
GH_TOKEN=$(gh auth token)

# Non-root user (their own install — no orca user created):
ssh user@host "GITHUB_TOKEN=$GH_TOKEN sh -s -- --version v0.0.3-rc.12 --prerelease" \
  < scripts/install.sh

# Root (creates orca user, drops privs, bootstraps daemon):
ssh root@host \
  "GITHUB_TOKEN=$GH_TOKEN ORCA_ADMIN_PUBKEY=\"$(cat ~/.ssh/id_ed25519.pub)\" \
   sh -s -- --version v0.0.3-rc.12 --prerelease" \
  < scripts/install.sh
```

The root variant runs `orca daemon install` automatically at the end of the
script, so a single ssh produces a fully running host.

If the host has `wget` but no `curl`, install.sh auto-detects and uses wget.
You don't pass anything.

---

## Path 2 — Push install (controller ships bytes)

Use when pull fails: no `curl`/`wget`, no GitHub reach, or air-gapped.

```sh
scripts/deploy-host.sh root@baldur
scripts/deploy-host.sh root@freyr
```

`deploy-host.sh`:

1. Resolves the latest RC tag (`--version` overrides).
2. Fetches the matching binary + sha256 via `gh release download` on the controller, cached under `$TMPDIR/orca-deploy-<version>/`.
3. SSHes into the host, probes `uname -s -m` + libc, scp's binary + sha + install.sh into `/tmp/`.
4. Runs `install.sh --from-file --admin-pubkey "$(reads ~/.ssh/id_*.pub)"`.

Target requirements: `sh`, `mv`, `chmod`, `mkdir`, `sha256sum`/`shasum`. **No
curl, no wget, no outbound network.**

---

## Verify

```sh
ssh orca@host '~/.local/bin/orca --version'
ssh orca@host '~/.local/bin/orca daemon status'
ssh orca@host 'journalctl --user -u orca -n 20 --no-pager'
```

Reachability from the controller:

```sh
curl -sS http://host:12000/api/health   # → {"ok":true}
```

Expect `listening on 0.0.0.0:12002 (mTLS)` in the journal — that's the
plugin-host. If it's missing or you see `server cert not found`, run
`orca pki ca-init` then `systemctl --user restart orca` as the orca user.

---

## Upgrading a host

Re-run the same path with a new `--version`. systemd picks up the new binary
on restart:

```sh
# Pull, root host:
ssh root@host "GITHUB_TOKEN=$GH_TOKEN ORCA_ADMIN_PUBKEY=\"$(cat ~/.ssh/id_ed25519.pub)\" \
  sh -s -- --version v0.0.3-rc.13 --prerelease" < scripts/install.sh
ssh orca@host 'systemctl --user restart orca'

# Push:
scripts/deploy-host.sh root@host --version v0.0.3-rc.13
ssh orca@host 'systemctl --user restart orca'
```

`daemon install` does not need to re-run unless the systemd unit shape
changed.

---

## Channel pinning

`install.sh` writes `~/.orca/channel` (or `/var/lib/orca/.orca/channel`) to
either `stable` or `rc` based on the tag shape (`-rc.` → `rc`). Pass
`--prerelease` to override.

---

## Known gotchas

- **`GITHUB_TOKEN` required for pull mode.** Releases are private. Not
  needed in push mode (controller has it via `gh`).
- **`--admin-pubkey` required when first creating the orca user.** Without
  it the controller would have no way to ssh back in as orca.
- **`PATH` on non-login shells.** `~/.local/bin` and `/var/lib/orca/.local/bin`
  are usually not on the SSH non-login PATH. Always invoke `~/.local/bin/orca`
  by absolute path in scripts.
- **First-boot plugin-host warning on rc.11 and earlier:** older
  `daemon install` did not run `pki ca-init`. One-time fix: `orca pki ca-init
  && systemctl --user restart orca` as the orca user. Fixed in tree for rc.13+.
