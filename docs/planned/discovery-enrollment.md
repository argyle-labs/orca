# Discovery + enrollment — scope

Phases 2 and 3 of host onboarding. Phase 1 (install) is at
[install-bootstrap.md](install-bootstrap.md).

```
   install            discovery           enrollment
   ──────             ─────────           ──────────
                      mDNS broadcast      out-of-band token
                      daemon announces    operator pastes
                      pod members         "orca pod add"
                      auto-list           establishes mTLS
                      candidates          trust + identity
                                          escrow

   ─ pre-enrollment ─ │ ─ pod member ─
```

Until enrollment, a host is a **candidate** — runs orca locally with
local-only state, participates in no pod operations. Discovery makes
candidates *visible* to pod members; enrollment makes them *part of*
the pod.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Discovery — mDNS broadcast

**Zero-config.** Every installed daemon announces itself on the LAN via
mDNS / DNS-SD. Pod members poll the multicast group and aggregate
candidate lists. Trusted L2 segment assumed (see threat model below).

### Service definition

| Field | Value |
|---|---|
| Service type | `_orca._tcp.local.` |
| Port | host's configured mesh port (default 12002) |
| Multicast | 224.0.0.251:5353 (standard mDNS) |
| Announce on | install completion + daemon start + IP change |

### TXT record fields

| Key | Meaning |
|---|---|
| `v` | orca protocol version |
| `peer_id` | stable host fingerprint (anchored to `/etc/machine-id` per `project_peer_identity_churn.md`) |
| `host` | hostname |
| `os` | linux/darwin/freebsd |
| `arch` | x86_64 / aarch64 / armv7 |
| `enrolled` | `0` (candidate) or `1` (pod member) |
| `release` | orca version |

`peer_id` is **stable** across re-installs (until `--rotate`). A host
that was wiped and re-installed but keeps the same `/etc/machine-id`
shows up in discovery with its prior `peer_id` — the pod can recognize
it as "previously known."

### Verbs

| Verb | What it does |
|---|---|
| `orca pod discover` | List mDNS candidates on this segment (enrolled + unenrolled) |
| `orca pod discover --unenrolled` | Only `enrolled=0` candidates ready for `orca pod add` |
| `orca pod discover --known` | Candidates whose `peer_id` matches a prior member record |

---

## 2. Enrollment — out-of-band token

Operator-driven. Pasting the OOB token (printed by install) into a pod
member's CLI mints peer mTLS identity and pulls the host into reconciler
scope.

### The verb

```sh
orca pod add <addr-or-name> --token <oob-token>
```

- `addr-or-name` is the mDNS-discovered name or a literal IP/host.
- `--token` is the one-time, time-bounded token from install output.
  Default TTL **15 minutes**, single-use. Token state lives in the
  candidate's local config store; consumption rotates it.

What it does:

1. Opens an authenticated channel to the candidate using the token.
2. Performs the mTLS cert exchange — candidate gets a peer cert minted
   by the pod CA; pod members get the candidate's pubkey.
3. Records the candidate's `peer_id` + cert in the pod roster (mesh
   CRDT, replicated to all members).
4. **Escrows the host's identity-derived key** for DR per
   [backup-restore.md](backup-restore.md) §4.4 — k-of-n distributed
   across enrolled peers. **This is the only point at which escrow
   happens.** Install does not escrow.
5. Flips the candidate's mDNS TXT `enrolled` flag to `1`.
6. Triggers any reconcilers that scope this host into their target set.

### Token semantics

| Property | Behavior |
|---|---|
| Lifetime | 15 min default (`--ttl` override) |
| Use count | single |
| Storage | row in candidate's `orca.db` |
| On expiry | candidate's daemon rotates it automatically; new token printed in journal + `orca system pair-token show` |
| On `--rotate` re-install | new token, prior token revoked |

`orca system pair-token show` and `orca system pair-token rotate` are
the first-class management verbs (replacing journal-grep — see
ROADMAP §1.3).

---

## 3. Threat model

Discovery + enrollment assume a **trusted L2 segment**. Concretely:

- An attacker on the same broadcast domain can see mDNS advertisements
  (so they learn that orca hosts exist + their peer_ids). This is
  acceptable — peer_id alone grants nothing.
- The OOB token is the trust anchor. An attacker who intercepts the
  token *and* reaches the candidate before the operator can race
  enrollment. Mitigations: 15-min TTL, single-use, operator pastes
  from install output (not from a chat tool).
- On hostile networks (public WiFi, shared L2 with untrusted devices):
  install with `--no-mdns` (candidate does not broadcast); enrollment
  uses a direct IP + token. Reconcilers that need ongoing mDNS service
  discovery on that segment are unavailable until the host moves.
- mDNS spoofing: an attacker advertising `enrolled=1` with a fake
  peer_id reveals nothing — pod members validate against the CRDT
  roster, not the TXT record.

---

## 4. Re-discovery / re-enrollment

A host that was wiped and re-installed should be detectable as
"previously known":

- If `/etc/machine-id` survived (CT restore from PBS, OS reinstall that
  preserves it), `peer_id` matches the prior roster entry. mDNS shows
  the host with `enrolled=0` + matching `peer_id` — `orca pod discover
  --known` surfaces it for operator action.
- Operator decision: `orca pod rejoin <peer_id> --token <new-oob-token>`
  re-establishes the peer cert *and* recovers the escrowed identity
  key (k-of-n unlock from the existing pod). No data loss for the
  orca-native secret store on the rejoined host.
- If `/etc/machine-id` was rotated (fresh OS install), `peer_id` is
  new. Old roster entry stays as a tombstone for audit; new entry is
  a clean enrollment with no key recovery.

---

## 5. What's shipped, what's missing

### Shipped

- mDNS discovery + mTLS pairing + cert rotation in `projects/pod`.
- Pair-token mint + single-use validation.

### Missing

- TXT record fields documented above (current advertisement is
  minimal — needs `peer_id`, `enrolled`, `os`, `arch`, `release`).
- `orca pod discover` flag set (`--unenrolled`, `--known`).
- `orca pod add` as the enrollment-side verb (today's "pair" verbs
  are the underlying primitive; this is the operator-facing wrapper).
- Identity-key escrow at enrollment (k-of-n distribution across
  peers — depends on [backup-restore.md](backup-restore.md) §4.4
  founding-peer DR scope).
- `orca pod rejoin` for re-discovered known peers.
- `--no-mdns` install flag for hostile networks.
- First-class token management verbs (`show` / `rotate`) replacing
  journal-grep.

---

## 6. Relationship to other docs

- [install-bootstrap.md](install-bootstrap.md) — phase 1 (install) that
  emits the OOB token and starts the mDNS advertisement.
- [pki-lifecycle.md](pki-lifecycle.md) — the cert exchange details.
- [backup-restore.md](backup-restore.md) §4.4 — identity-key escrow,
  founding-peer DR.
- [secrets-identity.md](secrets-identity.md) — the orca-native backend
  whose key gets escrowed at enrollment.
- ROADMAP §1.3 — install + enrollment hardening exit criteria.
