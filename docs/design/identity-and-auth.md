# Identity and authentication subsystem

> Design of record for orca's centralized identity, authentication, and
> group-based authorization (Tier B1 in
> [`../planned/build-order.md`](../planned/build-order.md)). Prerequisite for
> every multi-user feature: media add-request approval, purchase spend-authority,
> and per-user credential brokerage. When this disagrees with code, code wins —
> fix this file.

## Scope

One sign-in for orca. A single principal authenticates once and receives access
to the orca surface and, where granted, to managed downstream services
(Navidrome, Jellyfin, Sonarr, Radarr, Lidarr, LazyLibrarian, Home Assistant,
networking tools). Authentication methods are pluggable and multiple methods bind
to one principal. Authorization is group-based with paired standard/admin groups.
The subsystem is greenfield-additive: it extends the existing `auth` domain
rather than replacing it.

## Current state (as-built)

The `auth` domain already provides primitives the subsystem builds on:

- `projects/auth/src/users.rs` — `ReplicaUser` (`id`, `username`,
  `username_lower`, `password_hash`, `role`, timestamps); single-role string
  model; `count_admins` guard.
- `projects/auth/src/sessions.rs` — server-side cookie sessions, 30-day sliding
  expiry.
- `projects/auth/src/api_tokens.rs`, `secrets.rs`, `pki.rs`.
- `projects/auth/src/oauth.rs` — daemon-as-OAuth-client PKCE flow (reused as the
  OIDC relying-party base).
- `projects/server/src/serve/auth_routes.rs`, `middleware.rs`
  (`require_auth`, `require_tool_role`, bootstrap).
- `projects/dispatch/src/tool_roles.rs` — `authorize()` / `satisfies()`.
- `projects/db/src/replication_ops.rs` — monotonic `(stamp_ms, op_id)` delete
  op-log; generic `apply_pending_deletes` (no protected-key guard yet).
- `projects/db/src/config_store.rs` — `host_owner` single-writer pattern.
- `projects/pod/src/cert_rotation.rs` — overlap-window rotation (template for
  JWT signing-key rotation).
- `projects/contract/src/secrets_backend.rs` — backend registry pattern (cloned
  for `AccountBackend`).

Net-new external crates required: `jsonwebtoken`, a WebAuthn library, an
OpenID-Connect client library.

## Model

### Principal and identities

A `principal` is the account. Authentication methods are `identity` rows keyed by
`(provider, external_subject)` bound to a principal:

- `local_password` — username + Argon2 password hash (migrated from
  `ReplicaUser`).
- `passkey` — WebAuthn credential (public key, credential id, sign count).
- `oidc:google`, `oidc:discord` — external IdP relying-party subjects.

One principal holds many identities. Any method authenticates to the same
principal and inherits the same group memberships.

The stable **FQDN over Tailscale** is a hard prerequisite: it is the single
origin for OIDC redirect URIs and the WebAuthn RP-ID. It must be fixed before the
first passkey is registered — a changed RP-ID invalidates every existing passkey.

### Account linking

Linking a new method to an existing principal is security-critical:

- Re-authenticate with an already-trusted method immediately before linking.
- No implicit auto-linking by shared email address.
- Unlinking never orphans a principal (the last remaining sign-in method cannot
  be removed).
- External-IdP compromise is a stated residual risk; high-risk actions may demand
  re-authentication regardless of session state.

### Groups and authorization

Groups are uuid-referenced, runtime-editable objects. Standard groups ship as a
seed but are owned by the user at runtime. Paired standard/admin groups:

| Standard | Admin | Standard grants | Admin adds |
|----------|-------|-----------------|------------|
| `media` | `media-admins` | media players (Jellyfin, Navidrome), read/stream, add-requests | Sonarr/Radarr/Lidarr/LazyLibrarian administration, add-request approval |
| `automation` | `automation-admins` | Home Assistant as standard user | HA management, MQTT, Zigbee2MQTT, ZWaveJS |
| `networking` | `networking-admins` | view networking | modify networking |

Group membership resolves the service access a principal receives and the actions
it may take (e.g. only `media-admins` approve add-requests).

Seeding rule: default groups seed **only on the first node of a fleet**. Joining
nodes inherit them via replication. Local-seeding on every node would resurrect
groups the user deleted, because mesh replication is last-write-wins with a
delete op-log.

### Consistency and revocation

Mesh replication is wall-clock LWW with no central store. Security-critical
authorization uses **freshness-SLA + fail-closed**: a node refuses high-risk
actions unless its replica synced within N seconds. Revocation uses a monotonic
per-principal epoch — a token carrying an epoch lower than the principal's current
epoch is rejected, so revocation propagates without waiting for token expiry.

A replication-layer protected-key guard prevents deletion of the root admin grant
and the loopback break-glass path (the current `apply_pending_deletes` has no such
guard).

### Session and forward-auth

- Server-side sessions continue for the orca web surface.
- A mesh-signed JWT plus a `/auth/verify` endpoint back Caddy `forward_auth`,
  fail-closed, load-balanced across orca nodes. Silent refresh keeps short TTLs
  from interrupting in-flight streams.
- JWT signing keys rotate on an overlap window (verify with both current and
  previous key during the window) mirroring `pod/src/cert_rotation.rs`.

### Downstream credential brokerage

An `AccountBackend` trait + registry (mirroring `StorageBackend` /
`SecretsBackend`) projects orca principals into downstream services. Two tracks:

- `delegate_oidc` — the service accepts orca as an OIDC provider (Phase 2).
- `manage_account` + `set_credential` — orca provisions and rotates a native
  account, then brokers the credential to the logged-in principal on demand.

Projection is event-driven with a periodic backstop, single owner-node per
`(service, account)`. Per-device app-passwords isolate credential exposure.

## Build order

Milestones gate on each other; steps within a milestone are PR-sized. Core seams
land before any plugin producer.

- **M1 — Principal + groups + authz core.** `principal` and `groups` tables
  (uuid-referenced); `AccountBackend` trait + registry; seed paired groups on
  first-node only; migrate `ReplicaUser` single-role to group membership.
- **M2 — Freshness-SLA + fail-closed + revocation epoch.** Freshness gate for
  high-risk actions; monotonic per-principal epoch; replication protected-key
  guard for root admin grant + loopback break-glass.
- **M3 — forward_auth (verify).** Mesh-signed JWT; `/auth/verify`; fail-closed at
  Caddy; load-balanced; silent refresh; JWT key rotation on overlap window.
- **M4 — Federated login + multi-method linking.** OIDC relying-party for Google
  and Discord (on the `oauth.rs` PKCE base); passkeys (WebAuthn, RP-ID fixed to
  the Tailscale FQDN); `identity` table binding many methods to one principal;
  re-auth-before-link; no email auto-link; unlink cannot orphan.
- **M5 — AccountBackend projection.** Event-driven reconcile + periodic backstop;
  single owner-node per `(service, account)`; the two projection tracks;
  per-device app-passwords.
- **M6 — OIDC provider + credential brokerage.** orca as OIDC provider for
  downstream services; per-user retrieval of own credentials for enabled apps;
  fixed WebAuthn RP-ID strategy across LAN / Tailscale / public origins.

Downstream: media add-request approval (Tier B2) consumes M1 groups/authz;
purchase spend-authority consumes M1 + M2; credential brokerage is M6.

## Risks

- **FQDN-before-passkey ordering.** Registering passkeys against an unstable RP-ID
  strands them on the next origin change. The Tailscale FQDN must be fixed first.
- **Seed resurrection.** Any local-seed path on a joining node re-creates deleted
  default groups. Seeding is first-node-only by construction.
- **Protected-key gap.** `apply_pending_deletes` currently deletes any key; the
  root admin grant and break-glass path need an explicit guard before authz
  depends on them.
- **External-IdP trust.** A compromised Google/Discord account would otherwise
  inherit full principal access; high-risk actions re-authenticate independently.
