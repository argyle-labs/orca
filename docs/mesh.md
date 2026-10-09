# The mesh

The **mesh** is a small mutual-trust group of orca systems. Members can ping
each other, replicate the mesh CA, federate state, and share secrets — only
after both sides have explicitly opted into trust.

This doc covers: what the mesh is, how to add a host, how trust works, how to
leave, and the security model.

## Quick reference

| You want to | Run |
|---|---|
| Start a brand-new mesh (one-time, on the "first" host) | `orca system join --action init` |
| See what's on the network | `orca system list --discovery` |
| See incoming pairing offers on this host | `orca system list --pending` |
| Accept an offer | `orca system join --action accept <6-char-code>` |
| List mesh members | `orca system list` |
| Promote a peer to mutually-trusted | `orca system mesh update --action trust --peer-id <system-id> --on true` |
| Enable secrets storage on this host | `orca system mesh update --action settings --self-secure true` |
| Verify a peer end-to-end | `orca system health --id <system-id>` |
| Show cert expiry / rotation status | `orca system certs list` |
| Rotate the mesh CA (with overlap) | `orca system mesh update --action recover [--overlap-days 14]` |
| Leave the mesh (keep local data) | `orca system mesh delete --action leave` |
| Leave + wipe stored secrets | `orca system mesh delete --action leave --wipe-secrets` |
| Leave + factory-reset (everything but binary + bootstrap identity) | `orca system mesh delete --action leave --wipe-all` |

A `<system-id>` is the system's UUID, never its hostname or address. Look one
up in the `id` column of `orca system list`. To run a verb with no id argument,
such as `orca system certs list`, on another system, address the call with the
interim selector (`--peer <system-id>` on the CLI, `peer` in MCP arguments, the
`X-Orca-Peer` header on REST) until verbs address systems by id themselves
(orca#815).

## What the mesh is

The mesh is one or more orca systems that have exchanged mesh-CA-signed client
+ server TLS certificates. Membership unlocks:

* **mTLS peer-to-peer**: every mesh-internal call (e.g. `mesh/ping`,
  `mesh/notify-trust`, federated tool calls) is mTLS-authenticated by the
  shared mesh CA.
* **Mutual-trust state**: each host tracks a per-peer `(local_secure,
  peer_secure)` pair. When both bits are true, that peer is *mutually
  trusted* and the mesh CA private key replicates to them — promoting them
  from "member" to "inviter".
* **Secrets gate**: a host only stores secret material when its own
  `self_secure` flag is on. Joiners default to off so a freshly-paired
  device can't silently absorb credentials.

The mesh has no central server. Every member is symmetric once they hold the
mesh CA key. The first host is just the one that started the mesh.

## Joining the mesh

The fast path is fully automatic on a shared LAN.

1. **Start orca on the joiner.** On first boot it generates a per-host
   Ed25519 *bootstrap key* and begins advertising itself on mDNS
   (`_orca._tcp.local.`) as `unclaimed`.
2. **Any secure mesh member on the LAN sees the advertisement** and
   automatically pushes a `mesh/offer` over the bootstrap channel (a
   dedicated TLS SNI, `mesh-bootstrap.orca.local`, that doesn't require a
   client cert). The offer carries the mesh CA cert, the mesh id, and the
   hash of a 6-character pairing code. The inviter prints the code in its
   daemon log so the user can read it on screen.
3. **On the joiner, `orca system list --pending`** shows the offer.
4. **`orca system join --action accept <code>`** dials the inviter (TLS pinned to the
   inviter's bootstrap pubkey from the offer), sends two CSRs (client +
   server) plus the raw code, and receives signed certs back. The joiner
   installs the certs, records the inviter in `mesh` peer rows, and is now a
   full mesh member.

After this:

* `orca system health --id <inviter-id>` should succeed both directions
  (`orca system list` shows the inviter's `id`).
* `orca system list` on both hosts shows the other.
* Secrets storage on the joiner is **off** until the user opts in with
  `orca system mesh update --action settings --self-secure true`.

### Manual fallback (no mDNS)

mDNS is link-local. If the joiner is on a different subnet, blocked by a
firewall, or you just want to be explicit:

* On the joiner: `orca system join --action connect <ip[:port]>` — asks the addressed host
  whether there's an offer for this joiner.
* On the inviter: `orca system join --action offer <ip[:port]>` — pushes an offer to a
  specific address.

Both commands take `host`, `ip`, `host:port`, `ip:port`, or
`[ipv6]:port` — the default port is the orca plugin port (12002 on
default installs).

> Cross-subnet *discovery* (gossiping offers across already-paired peers)
> is on the roadmap. v1 supports cross-subnet pairing only via the manual
> address path above.

## Trust promotion

After pairing, a peer is *known* but not yet *trusted*. To unlock
CA-replication and the ability to invite further hosts, both sides must
flag each other secure:

```
# On hotel (foxtrot's id from `orca system list`):
orca system mesh update --action trust --peer-id 019f9f7b-2222-7e40-9e30-4987d8d12dcb --on true

# On foxtrot (hotel's id):
orca system mesh update --action trust --peer-id 019f9f7b-1176-7e40-9e30-4987d8d12dcb --on true
```

The moment both bits are true, the host that already has the mesh CA
private key pushes it to the peer via `mesh/push-ca-key` over the existing
mTLS channel. From that point, the newly-trusted host can extend its own
offers (`can_invite=1` in its mDNS advertisement).

`orca system mesh update --action trust --peer-id <system-id> --on false` reverses the local flag and notifies the
peer; mutual-trust falls back to false on both sides.

## self-secure (secrets gate)

```
orca system mesh update --action settings --self-secure true        # secrets writes enabled on this host
orca system mesh update --action settings --self-secure false       # secrets writes refused
orca system mesh update --action settings      # current state
```

`orca system join --action init` flips it on for the first host. Joiners start with it
off. Code paths that write to the `secrets` table consult this flag and
refuse if it's off, which prevents a freshly-paired device from
inadvertently mirroring credentials before its operator has reviewed the
mesh's posture.

## Leaving the mesh

Three flavors, in increasing destructiveness:

```
orca system mesh delete --action leave                  # notify peers, drop mesh PKI + mesh tables
orca system mesh delete --action leave --wipe-secrets   # above + TRUNCATE secrets
orca system mesh delete --action leave --wipe-all       # above + plugin_data, oauth_tokens,
                                # plugin_credentials, profile_credentials
```

All three preserve:

* The orca binary and its config (`~/.orca/orca.toml`).
* This host's **bootstrap Ed25519 key** (its long-lived identity) and
  the orca database file itself (encryption key included).
* Docs, agents, plugins (the *code*, not their stored data unless
  `--wipe-all`).

Leaving sends `mesh/peer-leaving` to each known peer; peers mark this
host as departed in their `mesh` peer rows row and refuse future mTLS until
re-paired. This is the clean exit. Network-partitioned peers will pick
it up the next time they observe the departed host failing to authenticate.

A host that has left can re-join later via the same automatic pairing
flow — the bootstrap identity is preserved, so a returning host appears
to peers as the *same* identity (their `mesh` peer rows row will exist as
`departed`, re-pairing clears the marker).

> **On names.** The mesh used to be called a "pod". That concept is gone —
> from the CLI, the crates, the wire frames (`mesh/ping`, `mesh/offer`, …), the
> TLS SNI values (`mesh.orca.local`), and the tables. Two transition
> affordances remain, both with a deletion trigger: the listener still ANSWERS
> to the old SNI, and every mesh cert is issued carrying both DNS names, so a
> system still on a pre-rename build can reach one that has upgraded. A daemon
> whose cert predates the rename reissues it on its first rotation tick, so the
> fleet converges without operator action. Once every system has started on a
> build carrying this change, `LEGACY_SERVER_SAN` / `LEGACY_BOOTSTRAP_SAN` and
> the dual-SAN issuance can go.
>
> **Rolling this release:** the wire frame names and the signed caller token
> changed together, so a new controller cannot drive a pre-rename system, and
> vice versa. Roll every system in one pass. A system that is powered off
> during the roll (hemlock) stays on the old names until it is brought up and
> rolled.

## Security model

### Identities

| Identity | Algorithm | Lifetime | Purpose |
|---|---|---|---|
| Bootstrap key | Ed25519 | host-lifetime | Pre-mesh identity; backs `mesh-bootstrap.orca.local` TLS cert and signs offer/confirm envelopes |
| Mesh CA | Ed25519, **1y** validity | per-rotation (manual) | Signs all mesh member certs |
| Peer client cert | Ed25519, **30d** validity | auto-rotated daily when <7d remaining | Authenticates outbound mTLS dials |
| Peer server cert | Ed25519, **30d** validity | auto-rotated daily when <7d remaining | Authenticates inbound `mesh.orca.local` SNI |

All certs are Ed25519 — modern, fast, constant-time, side-channel-resistant
by construction. SHA-512 is built into the signature; SHA-256 is used for
fingerprints and pairing-code hashes.

### TLS

* All channels are TLS 1.3 only. AEAD ciphers only. No fallback.
* mTLS on `core.orca.local` (plugin surface) and `mesh.orca.local` (mesh
  surface, mesh-CA-anchored).
* No client cert on `mesh-bootstrap.orca.local` — application-layer
  signed envelopes (Ed25519 signature over canonical JSON, embedded
  pubkey for fp lookup) authenticate the sender instead.

### Trust anchors at pairing time

The joiner has no CA when it accepts its first offer. Trust is anchored
on:

1. **mDNS-advertised bootstrap pubkey fingerprint.** The inviter
   publishes its bootstrap-key fingerprint as a TXT record. The joiner
   pins the inviter's bootstrap-TLS cert to that fp when dialing back
   for `mesh/join-confirm`.
2. **Signed offer envelopes.** The offer payload itself is Ed25519-signed
   by the inviter's bootstrap key. The joiner verifies the signature
   matches the same fp before recording the offer.
3. **The pairing code.** The 6-char code is the human-verification
   anchor: same code visible on both screens means the user has the
   right two hosts. The code is single-use (one accept consumes the
   pending offer) and short-lived (10 minute default TTL).

Compromise of any single one of these is not enough on its own:
* Spoofed mDNS without the bootstrap private key → cert pin fails.
* Forged offer without the bootstrap private key → signature check fails.
* Correct cert + signature but wrong code → server refuses to sign CSRs.

### What `mutual_secure` unlocks

Reaching mutual-trust with a peer:

* Replicates the mesh CA private key to that peer (`mesh/push-ca-key`).
* Lets that peer flip its mDNS advertisement to `can_invite=1` (when
  combined with `self_secure=on`), so it auto-offers to any new
  unclaimed orca on its LAN.
* Is the prerequisite for any federation primitive that handles
  sensitive material across hosts.

## Cert rotation

Both leaf certs (peer client/server) and the mesh CA rotate. The mechanisms
are different.

### Leaf certs (auto, seamless)

Peer certs are issued for **30 days**. A daily scheduler checks every cert
on disk; if any has less than **7 days** remaining, it's reissued:

* **Secure peers** (`has_mesh_ca_key`): self-sign locally. No network.
* **Non-secure peers**: dial any mutually-trusted peer with the CA key,
  call `mesh/refresh-cert` with fresh CSRs, install the returned certs.

The TLS resolver reads cert+key from disk on every handshake, so rotation
is seamless — `atomic_write_pem` does a tmp-write + `rename(2)`, which
means readers see either the old file or the new file but never a
half-written one. In-flight connections finish on whatever cert they
started with; new connections pick up the fresh cert immediately. **Zero
connections dropped.**

5-minute `not_before` backdate on every issued cert absorbs reasonable
clock skew across the mesh.

### Mesh CA (manual, with overlap)

```
orca system mesh update --action recover [--overlap-days 14]
```

This is a more deliberate operation. It:

1. Slides the current CA into the **previous** slot
   (`mesh/ca.previous.{cert,key}.pem`).
2. Generates a fresh CA and writes it to the **current** slot.
3. Re-issues this host's own peer certs under the new CA immediately.
4. Replicates both slots + the overlap deadline to every mutually-secure
   peer via `mesh/push-ca-state`.

During the overlap window, both CAs are in every peer's trust store — old
peer certs (signed by what's now `previous`) and new peer certs (signed
by `current`) all validate. Peer leaf certs auto-refresh under the new CA
on their normal rotation schedule. When the deadline expires, the daemon
drops the previous slot from disk and trust automatically (see
`mesh_self.ca_previous_expires_at`).

### What you can verify

```
orca system certs list
```

Shows days-remaining for the CA, mesh server, mesh client, and bootstrap
TLS certs, with rotation status (ok / due / EXPIRED). Run on every host
in the mesh after a rotation event to confirm everyone caught up.

## Troubleshooting

**`system list --pending` is empty even though I started the daemon.**
Verify the daemon log says `[mesh] mDNS responder + discoverer up` and
`[mesh] auto-offer scheduler armed`. If mDNS is blocked on your LAN
(some "guest" VLANs do this), use `orca system join --action offer <joiner-addr>` from a
secure peer.

**`system join --action accept <code>` says "no pending offer matches".**
* Code typo? Codes are case-sensitive and use Crockford-style
  base32 (no I/L/O/U).
* Code expired? Default TTL is 10 minutes; rerun the discovery cycle
  by toggling state with `system list --discovery`. (A new offer will be pushed on
  the next scheduler tick.)
* Inviter restarted? A daemon restart drops in-memory offer state but
  not DB rows; check `system list --pending` is still showing the offer.

**pairing succeeds but `system health --id <id>` fails.**
Almost always a clock skew issue — the certs have `not_before` set to
the inviter's wall time. NTP should keep this aligned. Confirm with
`date -u` on both hosts.

**`system mesh update --action trust` reports "notify-trust dial failed".**
The trust bit is set locally regardless. Peer will pick up the update
the next time it succeeds in dialing this host, or when the user re-runs
retry the trust call after the network heals.

**Wrong port?**
mDNS TXT carries the actual port the peer is listening on; commands
that take `<addr>` accept `host:port` so you can override the default.

**Different subnets?**
Use `system join --action offer <ip[:port]>` from the inviter side. Cross-subnet
auto-discovery is on the roadmap (will gossip offers between
already-paired peers).
