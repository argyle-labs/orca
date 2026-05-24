# PKI lifecycle — CA, peer certs, revocation

Orca's pod mesh already runs on mTLS (per orca-v1-scope §1
"Pod mesh — mTLS, mDNS discovery, cert rotation" — Prod). This doc
fills in the operational gaps: how the CA is rotated, how a peer
cert is revoked, what happens when a host is compromised, and how
trust-tier downgrade interacts with the mesh.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. What exists today

- **Pod CA**: each pod has a root CA generated at pod creation. The
  founding peer holds the CA private key.
- **Peer certs**: issued by the CA at pairing time. Cert chain is
  CA → peer cert → bound to peer_id.
- **Auto-rotation**: peer certs rotate before expiry (default lifetime
  is short — weeks not years — to keep blast radius small).
- **No revocation**: today, a compromised peer's cert just rotates
  out when it expires. There's no fast invalidation.

This doc adds revocation, CA rotation, and the response procedure
for a known compromise.

---

## 2. Revocation

### 2.1 Mechanism

Each pod maintains a **revocation set** — a small CRDT keyed by
peer_id with the revocation timestamp. The set is replicated across
all peers via the existing mesh sync. Verification:

- On every mTLS handshake, the verifier checks the presented cert's
  peer_id against the revocation set. Hit → handshake fails.
- The revocation set is signed by the CA (or a designated
  revocation key — see §3) so a compromised peer can't unrevoke
  itself.

OCSP/CRL endpoints are *not* used — they require an extra HTTPS
hop that complicates the bootstrap. The CRDT approach reuses the
mesh's existing trust channel.

### 2.2 Latency

Revocation propagates as fast as the mesh sync (seconds in a
healthy mesh, minutes worst case). For a *known active compromise*,
operators can also push the revocation to specific peers directly:

```sh
orca system peer revoke --peer-id <id> --reason "compromise"
orca system peer revoke --peer-id <id> --push frigg,thor,baldur
```

The `--push` form makes targeted peers re-verify all open
connections immediately rather than waiting for next handshake.

### 2.3 Audit

Every revocation writes to the audit DB (1-year retention, per
[observability.md](observability.md) §4.3). Fields: peer_id, time,
reason, operator (signing identity), affected peers (which peers
acknowledged).

---

## 3. CA rotation

### 3.1 When

- **Scheduled**: every N years (default 5). Slow enough to be rare,
  short enough that the old key isn't lying around forever.
- **On compromise**: founding peer's disk read or CA key handling
  failure → immediate rotation.
- **On pod split / merge**: structural changes that warrant a fresh
  trust anchor.

### 3.2 How (zero-downtime)

1. **Mint new CA** on the founding peer. Old CA remains active.
2. **Cross-sign**: new CA signs old CA, and vice versa. Every peer
   now trusts certs from either CA.
3. **Re-issue peer certs** under the new CA on next rotation
   (within the normal rotation window). Verifiers accept either
   chain during the overlap.
4. **Old CA retires** after a defined overlap period (default
   30 days). Cross-signing is removed; certs not yet re-issued
   fail to validate; the daemon refuses to start until paired
   under the new CA.
5. **Old key destroyed**.

The rotation is driven by `orca system pki ca rotate --plan` then
`--apply`. Plan output names every peer that hasn't re-issued yet
and the deadline.

### 3.3 Designated revocation key

The CA key itself is rarely-touched and ideally lives offline (or
in a hardware token on the founding peer). To avoid bringing it
online for routine revocations, the CA signs a **revocation-signing
key** that's used to sign the revocation set. The CA can revoke
the revocation-signing key if needed (the nuclear option).

---

## 4. Compromise response procedure

Step-by-step for the "we think peer X is compromised" case:

1. **Revoke** the peer cert: `orca system peer revoke --peer-id X --push <all reachable peers>`.
2. **Audit log** the event (automatic) with `reason="suspected compromise"`.
3. **Quarantine**: the orca CLI/UI marks X as `quarantined`. No new
   mesh operations route through it. Existing connections terminate
   on next handshake.
4. **Trust-tier downgrade** (automatic side effect): X drops to
   `trust = "compromised"` — a tier below `insecure`. Shadow hashes
   present on X are considered leaked; rotate every user's password
   ([orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6a).
5. **Cycle secrets**: any secret X had access to via the orca
   secrets store is rotated. The store records per-secret which
   peers had ever fetched it.
6. **Forensic**: X's logs (still in the audit DB, replicated)
   remain queryable from other peers even after X is offline.
7. **Re-pairing**: if X is to be returned to service, it's wiped
   and re-paired as a fresh host via [install-bootstrap.md](install-bootstrap.md).
   The old peer_id is retired permanently — the revocation entry
   stays in the set forever.

### 4.1 What trust-tier downgrade alone does NOT do

The credential-sync trust-tier model ([orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6a)
*demoting* a host from `secure` to `insecure` wipes shadow state
on the next reconcile, but it does **not** revoke the peer cert.
That's the right separation: demotion is an operational change
(this host is no longer trusted for credential storage),
revocation is a security event (this host's identity is compromised).

`compromised` is a third tier that does both.

---

## 5. Cert observability

Visible via `orca system pki status`:

- CA cert: expiry, days remaining, fingerprint.
- Per-peer cert: expiry, rotation timestamp, days since last rotate,
  bound peer_id.
- Revocation set size and last-sync timestamp.
- Cross-sign status during rotation windows.

Surfaced in the UI under each peer's detail panel; metrics graphed
in [observability.md](observability.md):

- `pki.peer.cert.days_until_expiry` per peer.
- `pki.revocation.set.size`.
- `pki.handshake.failures` by reason (revoked, expired, unknown CA, …).

Alerts fire when:

- Any peer cert < 7 days from expiry.
- CA cert < 90 days from expiry.
- Handshake failure rate spikes.
- A peer hasn't acknowledged the latest revocation set within 1h.

---

## 6. Open questions

- **CA key storage**: file on the founding peer? Hardware token
  (YubiKey)? TPM-bound? Recommend: hardware token if available,
  encrypted file backed up to offline media otherwise. Track in
  [backup-restore.md](backup-restore.md).
- **What about multi-pod federation?** If two pods peer (e.g.,
  homelab pod + cloud pod), each has its own CA. Federation needs
  a cross-CA trust bundle. Out of scope here; track separately
  once a second pod exists.
- **Hardware tokens for peer certs?** Overkill for hosts; relevant
  for human operators authenticating to the CLI. Track in the
  human-auth doc (when written).
- **What if the founding peer dies?** Without the CA key, no new
  peers can join and no CA rotation is possible. See
  [backup-restore.md](backup-restore.md) for CA key backup
  requirements; without that backup, the only path forward is
  rebuilding the pod from scratch.

---

## 7. Work breakdown

| # | Item | Size |
|---|---|---|
| K1 | Revocation set CRDT + mesh sync | M |
| K2 | Handshake verifier checks revocation set | S |
| K3 | `orca system peer revoke` with `--push` targeting | S |
| K4 | CA cross-sign rotation flow + `pki ca rotate --plan/--apply` | L |
| K5 | Revocation-signing key separate from CA key | M |
| K6 | `compromised` trust tier + downgrade procedure | M |
| K7 | Secret-rotation hook on peer revocation (which secrets X touched) | M |
| K8 | `orca system pki status` + observability metrics + alerts | M |
| K9 | Docs: compromise response runbook | S |

K1+K2 are the minimum viable revocation. K4 is a separate effort,
not blocking. K5 lands with K4.

---

## 8. Relationship to other planned docs

- [orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6a — trust
  tiers; this doc adds `compromised`.
- [install-bootstrap.md](install-bootstrap.md) — pairing flow; this
  doc covers what happens when a paired peer is revoked.
- [observability.md](observability.md) — surfaces cert state and
  revocation events.
- [backup-restore.md](backup-restore.md) — CA key backup.
