# The mesh

The **mesh** is a small group of orca systems that trust each other because
they are members. Members can ping each other, federate state, and share
secrets; secure members also hold the mesh CA.

This doc covers: what the mesh is, how to add a host, the secrets tiers, how to
leave, and the security model.

## Quick reference

| You want to | Run |
|---|---|
| Start a brand-new mesh (one-time, on the "first" host) | `orca system join --action init` |
| See what's on the network | `orca system list --discovery` |
| See incoming pairing offers on this host | `orca system list --pending` |
| Accept an offer | `orca system join --action accept <6-char-code>` |
| List mesh members | `orca system list` |
| Promote a member to secure (run on a secure member) | `orca system mesh tier update --action promote --id <system-id>` |
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
  federated tool calls) is mTLS-authenticated by the shared mesh CA.
* **Membership is trust**: a peer is trusted when its leaf cert chains to the
  mesh CA, is unexpired, carries the CN pinned to that member, and the member
  has not departed or been forgotten. There are no per-peer trust levels.
* **Secrets tiers**: every member is `secure` or insecure. An insecure
  member holds only its own secrets (names under a plugin installed on it);
  a secure member may also hold other members'. Joiners start insecure.

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
* The joiner is **insecure** until a secure member promotes it with
  `orca system mesh tier update --action promote --id <joiner-id>`.

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

## Secrets tiers (secure / insecure)

```
orca system mesh tier update --action promote --id <system-id>   # on a secure member holding the mesh CA key
orca system mesh tier update --action demote                     # demote this member
orca system mesh update --action settings                        # current tier
```

* **insecure** (default for every joiner): stores and resolves only its own
  secrets — names of the form `<plugin>.<...>` where `<plugin>` is installed
  on this member (e.g. `opnsense.main.api_key` on the member running the
  opnsense plugin). Any other name is refused, and it never serves secret
  values (`secrets.detail`) to mesh peers.
* **secure**: may additionally hold other members' secrets and act as a
  secret source for peers.

A member cannot promote itself. `promote` runs on a secure member holding the
mesh CA key: it signs a short-lived grant (10 min) naming the target member
with the mesh CA key, and the target verifies that signature against its own
copy of the mesh CA cert before switching to secure. Membership alone confers
no promotion rights. Leaving the mesh removes the tier entirely.

Only a secure member ever holds the mesh CA private key. After a promotion the
promoter pushes the key to the new secure member, and demoting a member
deletes its copy. An insecure member found holding the key at startup deletes
it, unless no other member proves it holds the key. The last secure member
holding the CA key cannot be demoted.

`mesh/push-ca-key` and `mesh/push-ca-state` are accepted only when:

* the sender is a verified member (known, not departed, not forgotten);
* the receiver is secure;
* the push carries a proof signed with the mesh CA key the receiver pins,
  binding the method, the receiver, the sender, a 2-minute expiry, a nonce and
  the SHA-256 of the payload, so a changed byte, a relay to another member or
  a stale push is refused;
* the nonce has not been seen before (replay).

A CA state push must also be a rotation chained from the receiver's current
CA; it is signed with that current CA key.

The CA key goes only to the member a `promote` just granted. CA state goes
only to members that sign a fresh nonce with the current CA key. A member's
own claim to be secure never decides who receives key material, and a member
becomes secure only by accepting a CA-signed promotion grant.

### Bound envelopes

Every credential-changing mesh message is a bound envelope
(`utils::pki::BoundEnvelope`): a signature over a length-prefixed encoding of
the method, sender, receiver, nonce, expiry (at most 5 minutes) and the
SHA-256 of the payload. The receiver checks every field and records the nonce
in `mesh_seen_nonces`, which survives restarts and holds only UUID nonces that
expire within 15 minutes. Entries are pruned 5 minutes after they expire.
Running counters cap the store at 100,000 nonces in total and 10,000 per
sender. `mesh/exec` caller tokens may use at most 80,000, which leaves room for
credential methods. A caller-token nonce is recorded only after the user and
role checks pass, so a refused token does not use up store space.

| Path | Signed by | Receiver binding |
|---|---|---|
| `mesh/push-ca-key` | mesh CA key | receiver id + the promotion grant nonce the receiver consumed |
| `mesh/push-ca-state` | mesh CA key | receiver id |
| `mesh/replicate-push` | sender's pinned bootstrap key (active members only) | receiver id |
| `mesh/replicate-export` | exporter's pinned bootstrap key | requester id + the requester's nonce |
| `mesh/refresh-cert`, `mesh/refresh-cert-bootstrap` | requester's pinned bootstrap key | signer id |
| `mesh/exec` caller tokens | caller's pinned bootstrap key | target id; payload carries user, tool and args hash |

A clock-based refusal carries the receiver's time (`[receiver_time=…]`). The
sender records the offset for that member and signs again once using the
receiver's clock, so a member whose clock is minutes off can still refresh
its certificates, push CA material, and run `mesh/exec`.

Rollout: `mesh/ping` reports `bound_envelopes: true`. A member that
advertises support, or has sent a bound envelope, is marked in
`mesh_peers.bound_envelopes`, and from then on its unbound envelopes are
refused. Unbound envelopes from members that have never advertised support
are accepted until a CA holder runs
`orca system mesh update --action require_bound`. That switch is refused
while any active member has not yet advertised support. Once on, it is a
CA-signed, replicated, one-way policy, and every unbound envelope is refused.
Until then, `system.list` still emits `local_secure`/`peer_secure` = true on
joined rows for daemons that predate membership-as-trust; once the switch is
on, it stops.

### Secure registry

`mesh_secure_registry` is a replicated, append-only list of CA-signed grants
and revocations. `mesh_policy` (the require-bound switch) works the same way.
Each row is verified against the current or previous mesh CA when it is
merged from a peer (a row that fails is dropped) and again when it is read. A
replicated copy can never change or delete an existing row.

- `promote` records a grant. A secure CA holder that the registry does not
  list yet (a founder, or a holder from before the registry) registers itself
  at startup.
- `orca system mesh tier update --action demote --id <member>` (run on a CA
  holder) records a revocation. The member does not need to take part. As
  soon as a merge brings in its own revocation, it demotes itself and purges
  its CA key, with no restart needed.
- `orca mesh ca rotate` and the CA key push after a promotion go only to
  members that hold a grant with no revocation under the same or a newer CA,
  and that also prove the claim right then. For a rotation, the member must
  sign a fresh nonce with the outgoing CA key. For a key push, it must have
  consumed that promotion's grant.
- A revoked member can still sign with the CA key it kept, so it could forge
  a new grant for itself. To stop that, a revocation blocks every grant signed
  under the same CA, and only rotation lifts it. Rotation re-signs the grants
  still in force under the new CA, which the revoked member never receives.
  To promote a revoked member again, rotate the CA first.

### Recovering a locked-out member

If the mesh refuses a member (for example, it lost its bootstrap key, or a
binding change shut it out):

1. On a CA holder, run `orca system mesh delete --action forget --id <member>`.
2. On the same holder, run `orca system mesh update --action join_token`.
3. On the locked-out member, run
   `orca system mesh update --action rejoin --addr <holder> --token <token>`.

`rejoin` is local-only. It checks the token, drops this member's local mesh
membership, and joins again through the holder. Joining clears the forget
tombstone.

`system.mesh.tier.update` (`promote`, `demote`) is super-admin,
sensitive and local-only: it needs a verified second factor (the owner's local
CLI is exempt), an admin is refused, and a remote peer can never drive it over
`mesh/exec`. The target of a `promote` receives its grant through
`system.mesh.update --action settings --secure true`, which is an admin,
remote-callable verb. A grant's issue time tolerates 30 s of clock skew, and a member
refuses any grant consumed within one grant lifetime (10 min) of its last
demotion.

A `mesh/exec` caller token acts with the lower of the named user's role and
the token's role. Custody tools (`dispatch::tool_roles::is_custody`) are the
exception: tier and grant delivery, secret-value reads and grants, PKI issue,
and anything that installs or runs code on the recipient (`system.update`,
`plugin.update`, `plugin.create`, `guest.exec`, `files.update`, and any
`*.install`, `*.upgrade`, `*.engine_update` or `*.exec`). A call to one of
these acts with at most the `user` role unless the signing peer proves it holds
the mesh CA key by signing the token nonce with it. A `plugin.create
--action invoke` call is judged by the tool it invokes.

A demotion or the post-startup
CA-key purge counts another member as a CA holder only when it signs a fresh
nonce with the mesh CA key; a bare "has key" reply is not enough. A grant must
be relayed by the secure member that issued it. Rotating the mesh CA
invalidates every grant still in flight, since grants are verified against the
current CA cert; re-run `promote` after a rotation.
`orca system join --action init` makes the first host secure.

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

### Caller identity on `mesh/exec`

The recipient resolves a forwarded call's caller from the signed caller token
and its own replicated `users` table, and acts with the lower of the user's
role and the token's role. The token carries no `can_mutate` grant, so the
listener always builds that caller with `can_mutate: false`. A member who
holds `can_mutate` locally loses it over the mesh: admin-gated data-mutation
tools that the grant would allow locally are refused when forwarded to a peer.
Only an admin can run them across the mesh.

## Cert rotation

Both leaf certs (peer client/server) and the mesh CA rotate. The mechanisms
are different.

### Leaf certs (auto, seamless)

Peer certs are issued for **30 days**. A daily scheduler checks every cert
on disk; if any has less than **7 days** remaining, it's reissued:

* **Secure peers** (`has_mesh_ca_key`): self-sign locally. No network.
* **Non-secure peers**: dial any member with the CA key,
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
4. Replicates both slots + the overlap deadline to every secure member via
   a signed `mesh/push-ca-state`.

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

**Wrong port?**
mDNS TXT carries the actual port the peer is listening on; commands
that take `<addr>` accept `host:port` so you can override the default.

**Different subnets?**
Use `system join --action offer <ip[:port]>` from the inviter side. Cross-subnet
auto-discovery is on the roadmap (will gossip offers between
already-paired peers).
