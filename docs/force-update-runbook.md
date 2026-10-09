# Runbook: force-updating a misbehaving orca host

When a host is stuck on the wrong version, wedged mid-update, or otherwise
misbehaving, escalate through these levels **in order**. Each level is more
invasive than the last; stop as soon as the host reports the target version and
`pending_restart == null`. The default path is always the **encrypted
mesh** — SSH is the last resort (see [Mesh-first policy](#mesh-first-policy)).

Throughout, target the host by its **system id**, a UUID shown in the `id`
column of `orca system list`. Verbs that take `--id` route the call to that
system themselves; `--id` takes a UUID only and refuses a hostname. A verb with
no `--id` of its own, such as `system.certs.list`, reports on the system the
call is addressed to: address it to the host's id with the ambient selector
(`--peer <id>` on the CLI, `peer` in MCP arguments, the `X-Orca-Peer` header on
REST). That selector is interim, in place until verbs address systems by id
themselves (orca#815).

## Level 0 — Diagnose before you touch anything

Read-only probes:

- `system_update(id=<id>)` — omit all other args. Reports `current_version`,
  `channel`, `pinned_to`, `update_available`, `pending_restart`. A
  `-dev+<hash>.dirty` version means a hand-built binary, not a release.
- `system.certs.list`, addressed to the host's id — leaf/CA cert days-remaining
  and `self_secure` for that host. A leaf at `0` days is the cert-expiry
  deadlock (mesh handshakes fail; see [self-heal](#appendix-cert-expiry-deadlock)).
- `system.list` / `system.health --id <id>` — reachability,
  `local_secure`/`peer_secure`.
- Log scan on the host: `database is locked` (identity convergence failing),
  `certificate expired`, `TLS accept failed`.

## Level 1 — Normal mesh self-update

```
system_update(id=<id>, channel=beta)     # applies the channel's latest release
```

The host downloads its own target-triple asset over the mesh, sha256-verifies,
installs, and restarts. Verify: re-probe → `current_version == latest`,
`pending_restart == null`.

## Level 2 — Force a specific version (dirty / dev / pinned / stuck)

Symptom: host is on a `…-dev+…dirty` build, is pinned, or `update_available` is
`false` while running the wrong version.

```
system_update(id=<id>, version=<tag>)    # e.g. 0.1.1-rc.18
```

Passing an explicit `version` **clears any pin and applies that exact release** —
this is the lever that un-sticks a host from a hand-built/dirty binary. (A
token-less host is served the asset automatically by a token-holding peer via
`system_serve_release`.)

> Real example: a host was stuck on `0.1.1-rc.17-dev+gb012fb7.dirty` (a
> manually-scp'd binary). `system_update(id=<id>, version=0.1.1-rc.18)`
> returned `applied: 0.1.1-rc.18`, notes `["pin cleared", "applied ..."]`, and
> the supervisor auto-restarted onto the release. No SSH needed.

## Level 3 — Nudge the daemon if the restart didn't fire

If `pending_restart` persists (new binary staged but old one still running):

```
system_update(id=<id>, daemon=reclaim)     # or "stop" / "park"
```

This cycles the supervised daemon onto the staged binary. Re-probe to confirm.

## Level 4 — The id doesn't resolve (stale identity)

If `--id <id>` fails with "no active paired peer matches '<id>'", the caller's
roster holds the host under a stale identity, usually because
`converge_peer_identity` has been failing (look for `database is locked` in the
caller's log). Repair the identity rather than working around it: fix the
caller, let convergence complete (or re-pair the host), then read the host's
current id from `orca system list` and retry with `--id <id>`.

## Level 5 — SSH force-reinstall (LAST RESORT)

Only when the mesh path is genuinely unavailable: daemon down/wedged, or a
cert-expiry deadlock the mesh can't route around.

1. **Pick the right artifact for the host's libc** — a mismatch won't exec:
   - glibc (Debian, Bazzite, CachyOS): `x86_64-unknown-linux-gnu`
   - musl (Alpine): `x86_64-unknown-linux-musl` — a glibc binary fails with
     "No such file or directory" on Alpine (no `/lib64/ld-linux-*`).
   - macOS: `aarch64-apple-darwin` (Apple Silicon).
2. scp to `/tmp`, `sha256sum` against the release checksum, then
   `install -o orca -g orca -m 755 /tmp/orca.new /var/lib/orca/.local/bin/orca`.
3. Restart per the host's init system:
   - systemd: `sudo systemctl restart orca`
   - OpenRC (Alpine): `sudo rc-service orca restart`
   - supervise-daemon: `sudo kill <daemon-pid>` (supervisor respawns)
4. Keep the previous binary as `orca.bak-<date>` so you can revert; verify the
   daemon comes back and certs are valid before moving on.

## Mesh-first policy

Manual SSH updates are a **last resort**. The install/update flow is designed to
work over the encrypted mesh; if it doesn't, that's a bug to fix in the
update path, not a reason to reach for SSH. SSH bypasses sha-verification,
can't reach hosts your key isn't on, and is easy to get wrong (wrong libc).

## Verification (run after every level)

- `system_update(id=<id>)` → `current_version == target`, `pending_restart == null`.
- `system.certs.list`, addressed to the host's id → leaf certs healthy.
- `system.health --id <id>` / `system.list` → reachable, mutual-secure.
- Host log tail is clean (no lock / cert / handshake errors).

## Appendix: cert-expiry deadlock

A non-secure host whose mesh **leaf** cert expired can't mTLS-authenticate the
very refresh call that would renew it. rc.18+ self-heals via a bootstrap-channel
refresh (daily rotation tick). If a host is already deadlocked on an older
build, get the fixed binary onto it (Level 5 if the mesh can't reach it), then a
restart triggers the bootstrap refresh and the leaf renews.
