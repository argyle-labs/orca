# Runbook: force-updating a misbehaving orca host

When a host is stuck on the wrong version, wedged mid-update, or otherwise
misbehaving, escalate through these levels **in order**. Each level is more
invasive than the last; stop as soon as the host reports the target version and
`pending_restart == null`. The default path is always the **encrypted pod
mesh** — SSH is the last resort (see [Mesh-first policy](#mesh-first-policy)).

Throughout, target the host by its **`machine_id` (peer_id)** when it resolves;
fall back to **hostname** if identity convergence hasn't completed (see Level 4).

## Level 0 — Diagnose before you touch anything

Read-only probes (all peer-dispatchable):

- `system_update(peer=<id>)` — omit all other args. Reports `current_version`,
  `channel`, `pinned_to`, `update_available`, `pending_restart`. A
  `-dev+<hash>.dirty` version means a hand-built binary, not a release.
- `pod_detail(peer=<id>)` — leaf/CA cert days-remaining + `self_secure`. A leaf
  at `0` days is the cert-expiry deadlock (mesh handshakes fail; see
  [self-heal](#appendix-cert-expiry-deadlock)).
- `pod_list` / `pod_ping` — reachability, `local_secure`/`peer_secure`.
- Log scan on the host: `database is locked` (identity convergence failing),
  `certificate expired`, `TLS accept failed`.

## Level 1 — Normal mesh self-update

```
system_update(peer=<id>, channel=rc)     # applies the channel's latest release
```

The host downloads its own target-triple asset over the mesh, sha256-verifies,
installs, and restarts. Verify: re-probe → `current_version == latest`,
`pending_restart == null`.

## Level 2 — Force a specific version (dirty / dev / pinned / stuck)

Symptom: host is on a `…-dev+…dirty` build, is pinned, or `update_available` is
`false` while running the wrong version.

```
system_update(peer=<id>, version=<tag>)  # e.g. 0.1.1-rc.18
```

Passing an explicit `version` **clears any pin and applies that exact release** —
this is the lever that un-sticks a host from a hand-built/dirty binary. (A
token-less host is served the asset automatically by a token-holding peer via
`system_serve_release`.)

> Real example: `loki` was stuck on `0.1.1-rc.17-dev+gb012fb7.dirty` (a
> manually-scp'd binary). `system_update(peer=loki, version=0.1.1-rc.18)`
> returned `applied: 0.1.1-rc.18`, notes `["pin cleared", "applied ..."]`, and
> the supervisor auto-restarted onto the release. No SSH needed.

## Level 3 — Nudge the daemon if the restart didn't fire

If `pending_restart` persists (new binary staged but old one still running):

```
system_update(peer=<id>, daemon=reclaim)   # or "stop" / "park"
```

This cycles the supervised daemon onto the staged binary. Re-probe to confirm.

## Level 4 — Targeting fallback (identity not converged)

If a host won't resolve by `machine_id` ("no active paired peer matches
'<id>'") but resolves by hostname, its `pod_peers` identity on the caller is
stale — usually because `converge_peer_identity` has been failing (look for
`database is locked` in the caller's log; fixed by the busy_timeout change in
rc.18+). **Target by hostname** to get the update through; once the caller runs
the fix and convergence completes, `machine_id` targeting works again.

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

- `system_update(peer=<id>)` → `current_version == target`, `pending_restart == null`.
- `pod_detail(peer=<id>)` → leaf certs healthy.
- `pod_ping` / `pod_list` → reachable, mutual-secure.
- Host log tail is clean (no lock / cert / handshake errors).

## Appendix: cert-expiry deadlock

A non-secure host whose mesh **leaf** cert expired can't mTLS-authenticate the
very refresh call that would renew it. rc.18+ self-heals via a bootstrap-channel
refresh (daily rotation tick). If a host is already deadlocked on an older
build, get the fixed binary onto it (Level 5 if the mesh can't reach it), then a
restart triggers the bootstrap refresh and the leaf renews.
