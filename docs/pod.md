# Pod mesh

A **pod** is a small mutual-trust mesh of orca instances. Members can ping
each other, replicate the mesh CA, federate state, and share secrets — only
after both sides have explicitly opted into trust.

This doc covers: what a pod is, how to add a host, how trust works, how to
leave, and the security model.

## Quick reference

| You want to | Run |
|---|---|
| Start a brand-new pod (one-time, on the "first" host) | `orca pod init` |
| See what's on the network | `orca pod discover` |
| See incoming pairing offers on this host | `orca pod pending` |
| Accept an offer | `orca pod accept <6-char-code>` |
| List pod members | `orca pod list` |
| Promote a peer to mutually-trusted | `orca pod trust <peer-id> on` |
| Enable secrets storage on this host | `orca pod self-secure on` |
| Verify a peer end-to-end | `orca pod ping <host>` |
| Leave the pod (keep local data) | `orca pod leave` |
| Leave + wipe stored secrets | `orca pod leave --wipe-secrets` |
| Leave + factory-reset (everything but binary + bootstrap identity) | `orca pod leave --wipe-all` |

## What a pod is

A pod is one or more orca daemons that have exchanged mesh-CA-signed client
+ server TLS certificates. Membership unlocks:

* **mTLS peer-to-peer**: every pod-internal call (e.g. `pod/ping`,
  `pod/notify-trust`, federated tool calls) is mTLS-authenticated by the
  shared mesh CA.
* **Mutual-trust state**: each host tracks a per-peer `(local_secure,
  peer_secure)` pair. When both bits are true, that peer is *mutually
  trusted* and the mesh CA private key replicates to them — promoting them
  from "member" to "inviter".
* **Secrets gate**: a host only stores secret material when its own
  `self_secure` flag is on. Joiners default to off so a freshly-paired
  device can't silently absorb credentials.

A pod has no central server. Every member is symmetric once they hold the
mesh CA key. The first host is just the one that ran `pod init`.

## Joining a pod

The fast path is fully automatic on a shared LAN.

1. **Start orca on the joiner.** On first boot it generates a per-host
   Ed25519 *bootstrap key* and begins advertising itself on mDNS
   (`_orca._tcp.local.`) as `unclaimed`.
2. **Any secure pod member on the LAN sees the advertisement** and
   automatically pushes a `pod/offer` over the bootstrap channel (a
   dedicated TLS SNI, `pod-bootstrap.orca.local`, that doesn't require a
   client cert). The offer carries the mesh CA cert, the pod id, and the
   hash of a 6-character pairing code. The inviter prints the code in its
   daemon log so the user can read it on screen.
3. **On the joiner, `orca pod pending`** shows the offer.
4. **`orca pod accept <code>`** dials the inviter (TLS pinned to the
   inviter's bootstrap pubkey from the offer), sends two CSRs (client +
   server) plus the raw code, and receives signed certs back. The joiner
   installs the certs, records the inviter in `pod_peers`, and is now a
   full pod member.

After this:

* `orca pod ping <inviter>` should succeed both directions.
* `orca pod list` on both hosts shows the other.
* Secrets storage on the joiner is **off** until the user opts in with
  `orca pod self-secure on`.

### Manual fallback (no mDNS)

mDNS is link-local. If the joiner is on a different subnet, blocked by a
firewall, or you just want to be explicit:

* On the joiner: `orca pod connect <ip[:port]>` — asks the addressed host
  whether there's an offer for this joiner.
* On the inviter: `orca pod offer <ip[:port]>` — pushes an offer to a
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
# On mint:
orca pod trust peer.thor on

# On thor:
orca pod trust peer.mint on
```

The moment both bits are true, the host that already has the mesh CA
private key pushes it to the peer via `pod/push-ca-key` over the existing
mTLS channel. From that point, the newly-trusted host can extend its own
offers (`can_invite=1` in its mDNS advertisement).

`orca pod trust <peer-id> off` reverses the local flag and notifies the
peer; mutual-trust falls back to false on both sides.

## self-secure (secrets gate)

```
orca pod self-secure on        # secrets writes enabled on this host
orca pod self-secure off       # secrets writes refused
orca pod self-secure show      # current state
```

`orca pod init` flips it on for the first host. Joiners start with it
off. Code paths that write to the `secrets` table consult this flag and
refuse if it's off, which prevents a freshly-paired device from
inadvertently mirroring credentials before its operator has reviewed the
pod's posture.

## Leaving a pod

Three flavors, in increasing destructiveness:

```
orca pod leave                  # notify peers, drop mesh PKI + pod tables
orca pod leave --wipe-secrets   # above + TRUNCATE secrets
orca pod leave --wipe-all       # above + plugin_data, oauth_tokens,
                                # plugin_credentials, profile_credentials
```

All three preserve:

* The orca binary and its config (`~/.orca/orca.toml`).
* This host's **bootstrap Ed25519 key** (its long-lived identity) and
  the orca database file itself (encryption key included).
* Docs, agents, plugins (the *code*, not their stored data unless
  `--wipe-all`).

`pod leave` sends `pod/peer-leaving` to each known peer; peers mark this
host as departed in their `pod_peers` row and refuse future mTLS until
re-paired. This is the clean exit. Network-partitioned peers will pick
it up the next time they observe the departed host failing to authenticate.

A host that has left can re-join later via the same automatic pairing
flow — the bootstrap identity is preserved, so a returning host appears
to peers as the *same* identity (their `pod_peers` row will exist as
`departed`, re-pairing clears the marker).

## Security model

### Identities

| Identity | Algorithm | Lifetime | Purpose |
|---|---|---|---|
| Bootstrap key | Ed25519 | host-lifetime | Pre-pod identity; backs `pod-bootstrap.orca.local` TLS cert and signs offer/confirm envelopes |
| Mesh CA | Ed25519, 10y validity | pod-lifetime | Signs all pod member certs |
| Peer client cert | Ed25519, 2y validity | per-host | Authenticates outbound mTLS dials |
| Peer server cert | Ed25519, 2y validity | per-host | Authenticates inbound `pod.orca.local` SNI |

All certs are Ed25519 — modern, fast, constant-time, side-channel-resistant
by construction. SHA-512 is built into the signature; SHA-256 is used for
fingerprints and pairing-code hashes.

### TLS

* All channels are TLS 1.3 only. AEAD ciphers only. No fallback.
* mTLS on `core.orca.local` (plugin surface) and `pod.orca.local` (pod
  surface, mesh-CA-anchored).
* No client cert on `pod-bootstrap.orca.local` — application-layer
  signed envelopes (Ed25519 signature over canonical JSON, embedded
  pubkey for fp lookup) authenticate the sender instead.

### Trust anchors at pairing time

The joiner has no CA when it accepts its first offer. Trust is anchored
on:

1. **mDNS-advertised bootstrap pubkey fingerprint.** The inviter
   publishes its bootstrap-key fingerprint as a TXT record. The joiner
   pins the inviter's bootstrap-TLS cert to that fp when dialing back
   for `pod/join-confirm`.
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

* Replicates the mesh CA private key to that peer (`pod/push-ca-key`).
* Lets that peer flip its mDNS advertisement to `can_invite=1` (when
  combined with `self_secure=on`), so it auto-offers to any new
  unclaimed orca on its LAN.
* Is the prerequisite for any federation primitive that handles
  sensitive material across hosts.

## Troubleshooting

**`pod pending` is empty even though I started the daemon.**
Verify the daemon log says `[pod] mDNS responder + discoverer up` and
`[pod] auto-offer scheduler armed`. If mDNS is blocked on your LAN
(some "guest" VLANs do this), use `orca pod offer <joiner-addr>` from a
secure peer.

**`pod accept <code>` says "no pending offer matches".**
* Code typo? Codes are case-sensitive and use Crockford-style
  base32 (no I/L/O/U).
* Code expired? Default TTL is 10 minutes; rerun the discovery cycle
  by toggling state with `pod discover`. (A new offer will be pushed on
  the next scheduler tick.)
* Inviter restarted? A daemon restart drops in-memory offer state but
  not DB rows; check `pod pending` is still showing the offer.

**`pod accept` succeeds but `pod ping` fails.**
Almost always a clock skew issue — the certs have `not_before` set to
the inviter's wall time. NTP should keep this aligned. Confirm with
`date -u` on both hosts.

**`pod trust on` reports "notify-trust dial failed".**
The trust bit is set locally regardless. Peer will pick up the update
the next time it succeeds in dialing this host, or when the user re-runs
`pod trust` after the network heals.

**Wrong port?**
mDNS TXT carries the actual port the peer is listening on; commands
that take `<addr>` accept `host:port` so you can override the default.

**Different subnets?**
Use `pod offer <ip[:port]>` from the inviter side. Cross-subnet
auto-discovery is on the roadmap (will gossip offers between
already-paired peers).
