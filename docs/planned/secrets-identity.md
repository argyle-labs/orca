# Secrets + unified identity — scope

> **HARD RULE — personal 1Password only.** Orca's homelab secrets backend
> targets the **user's personal 1Password account** exclusively. **Never**
> the rebuy/work tenant or any vault the `rebuy auth op` CLI authenticates
> against. The 1Password backend in §2.1 must be configured with a
> *personal* service-account token, kept separate from any work-tenant
> integration. See `feedback_personal_1password_only`.

Trait insertion point: shipped at `projects/auth/src/secrets.rs`.
The `SecretBackend` work in §2.1 extends that file — no greenfield
crate.


Goal: orca becomes the **single source of identity and secrets** for the
homelab. One login (the user's orca account) authenticates everywhere —
SMB shares, hosts, services — and every secret resolves through orca,
backed by an external password manager (1Password first; Bitwarden /
Vaultwarden as alternative backends). Stop hand-distributing per-service
passwords and reused root/user passwords.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Why

- Today secrets are ad hoc: orca's own secret store has only an
  `inline` backend; the rebuy CLI can reach 1Password (`op`) but the
  desktop session expires every 10 min and is separate from orca's
  store; service passwords (e.g. the tyr SMB `pool` user) are unknown /
  unmanaged; root + user passwords are reused across hosts.
- The user wants **one identity**: their SMB username+password ==
  their orca username+password, and their orca (admin) account should
  have access to everything by nature of the role.
- Security goal: eliminate password reuse and rotate baseline
  root/user/orca passwords across all machines.

---

## 2. Two capabilities

### 2.1 Secret backend abstraction

A `SecretBackend` trait so orca can resolve/store secrets through a
configured provider. **All four backends are first-class v1 paths**
— operator picks per deployment (and may override per secret).
There is no "primary, others later" hierarchy:

```
SecretBackend (trait)
  ├── orca         (native — encrypted-at-rest in orca DB; host-derived key)
  ├── onepassword  (Connect / service-account token; personal tenant only)
  ├── bitwarden    (Bitwarden cloud via CLI session or API key)
  └── vaultwarden  (self-hosted Bitwarden-protocol instance)
```

The **orca-native** backend is a real, viable choice — not a dev
stub. It targets the case where the homelab wants zero external
dependencies, and serves as the break-glass store on the
control-plane node. The previous `inline` backend is the seed for
this; it gets promoted from "dev only" to "fully supported," with
key derivation tied to host identity (`projects/auth/src/host_identity.rs`),
mTLS-gated reads, and an export/import surface for migration.

Handle forms (the prefix selects the backend). Two granularities:

| Backend | Field handle | Bundle handle |
|---------|--------------|---------------|
| Orca-native | `orca://path/to/item/field` | `orca://path/to/item` |
| 1Password | `op://vault/item/field` | `op://vault/item` |
| Bitwarden / Vaultwarden | `bw://collection/item/field` | `bw://collection/item` |

**Bundles are the primary pattern** for per-system envs+secrets:
one item per host or service carries the entire env+secret set as
fields.

**Handle grammar — 1Password is exactly 3 segments.** 1Password's
`op://` URI is `op://<vault>/<item>/<field>`. Orca does **not**
extend that grammar. The visual grouping (`automations`,
`services`, `orca`) lives in the **item title** — the dot is part
of the title, not a URI separator. So the convention is:

| Logical purpose | Item title | Field-handle example | Bundle-handle example |
|---|---|---|---|
| Per-host envs+secrets | `automations.maple` | `op://Orca/automations.maple/SONARR_API_KEY` | `op://Orca/automations.maple` |
| Per-service envs+secrets | `services.sonarr` | `op://Orca/services.sonarr/api_key` | `op://Orca/services.sonarr` |
| Orca daemon per host | `orca.freyr` | `op://Orca/orca.freyr/peer_token` | `op://Orca/orca.freyr` |

Orca-native + Bitwarden/Vaultwarden mirror the same convention
within their own grammars:

| Backend | Field handle | Bundle handle |
|---|---|---|
| Orca-native | `orca://Orca/automations.maple/SONARR_API_KEY` | `orca://Orca/automations.maple` |
| Bitwarden / Vaultwarden | `bw://Orca/automations.maple/SONARR_API_KEY` | `bw://Orca/automations.maple` |

Within an item, **field type drives trust level** — `concealed`
(1P "password"-type) → projected as a secret (never logged, UI
masked); `string` → projected as a non-secret env. The other
backends mirror this typing (orca-native: per-field `secret: bool`;
Bitwarden: custom-field `type` 1=text, 2=hidden).

**Bundle declaration in the config repo:**

```toml
[[projection]]
target  = "host:maple"
bundle  = "op://Orca/automations.maple"
restart = "restart"   # default restart policy for fields w/o override
```

One declaration replaces N per-key entries. Adding a new env or
secret = adding a field to the item; orca picks it up via the
rotation-detection path (still user-gated per the hard rule).
Field-level overrides remain possible (per-field restart policy,
key rename).

Cross-cutting rules:

- Secrets are referenced by handle in config repos, never inlined.
- Session handling: 1Password desktop integration expires fast —
  orca uses **Connect server / service-account token** for
  unattended resolution. Bitwarden uses an API key, not a
  password-derived session. Vaultwarden follows Bitwarden.
  Orca-native has no session — direct DB read under mTLS.
- **Tenant separation (hard rule):** the 1Password backend uses
  the user's **personal** account only, never the rebuy/work
  tenant that `rebuy auth op` authenticates against.
- **Migration is supported, not a rewrite.** `orca secret migrate
  --from <backend> --to <backend>` walks every handle, re-stores
  the value in the new backend, swaps the handle prefix in config,
  and re-projects. Designed so an operator can change their mind
  about backends without losing the projection graph.
- `system_secret_backends` already lists kinds; today only
  `inline` is wired — the v1 work is filling in the other three
  adapters and promoting `inline` to `orca`.
- **Tenant separation (hard rule):** personal/homelab secrets use the
  user's **personal** 1Password account only — never the rebuy *work*
  tenant that the existing `rebuy auth op` CLI authenticates against.
  Orca's 1Password backend must be configured with a personal-account
  service-account token, fully separate from the rebuy integration.
- `system_secret_*` endpoints gain backend selection
  (`system_secret_backends` already lists kinds; today only `inline`).

### 2.2 Env + secret projection

All secrets are environment variables on the target; not all
environment variables are secrets. Same projection adapters, two
trust levels — secrets gated by the secret backend (handles only
in git, never logged), non-secret envs ride in cleartext. CLI:
`orca env` for non-secret, `orca secret` for privileged.

> **HARD RULE — user-triggered changes only.** Orca **never**
> auto-applies changes to envs, secrets, or system state. Drift
> detection + notification only; the operator decides when and
> what to apply. No self-healing, no auto-reproject, no scheduled
> apply. (See ROADMAP §1.11 + cross-cutting standing rules.)

#### 2.2.1 Per-node configuration

Every orca node is independently configured along three axes;
together they determine where secrets materialize and how
rotations propagate.

| Setting | What it controls |
|---------|------------------|
| `secrets.backends` | Which backends this node can call **directly** (e.g. mint laptop has `[op, orca]`; freyr has `[orca]` only). Determines what this node can resolve on its own without a peer. |
| `secrets.store_local` | Does the node keep secrets in its own encrypted store (`orca-native`)? When `on`, secrets can be **pushed to this node** by peers and persist locally. |
| `secrets.sync_peers` | Does the node accept pushes of secrets *owned by other systems* (e.g. a control-plane node mirroring fleet state)? Independent of `store_local`. |

#### 2.2.2 Topology patterns

Both patterns are first-class and coexist in one deployment.
Each system declares the resolution path it wants.

**Pattern A — gateway node fans out** (typical for 1Password):

- One node (e.g. **mint laptop**) has `secrets.backends = [op,
  orca]` — it's where 1Password is actually logged in.
- Other nodes (e.g. **freyr**, **baldur**, **thor**) have
  `secrets.backends = [orca]` only + `store_local = on`.
- The gateway resolves 1Password handles, then **pushes** the
  resolved values into target nodes' local orca-native store (or
  projects them directly onto the workload targets, depending on
  the declaration).
- When 1Password rotates, the **gateway** detects the change (it
  owns the 1Password connection), surfaces the pending-change
  notification, user runs `orca apply` on the gateway, gateway
  re-pushes to peers.
- Peer nodes never need direct 1Password access.

**Pattern B — direct backend access on the consuming node**:

- A node (e.g. **maple**) has `secrets.backends = [op, orca]`
  with 1Password configured locally.
- Maple **detects rotations on its own** via the 1Password
  detection mechanism (webhook / version-field poll).
- On rotation: maple emits a pending-change notification +
  alert; awaits `orca apply` on maple itself.
- No gateway hop. Useful when a host needs to operate
  independently or when latency from the gateway is a concern.

#### 2.2.3 Remote action without local backend access

A node with `secrets.backends = [orca]` and both toggles `off`
can still run any orca command that needs 1Password-backed (or
Bitwarden/Vaultwarden) secrets. Resolution path:

1. CLI on node A invokes `orca <service> <verb>` targeting host B.
2. Node A's daemon forwards the action over mTLS to a peer that
   *does* have the required backend (e.g. the mint gateway).
3. The gateway resolves the handle, opens an mTLS channel to host
   B, injects the value at write time, and zeros the buffer.
4. Node A sees only the outcome — never the value.

The CLI surface is identical on every node; only the resolution
path differs. There is no "diminished" mode.

#### 2.2.4 Declarations

Storing a secret is half the job. The other half is **projecting**
it onto the system that needs it — and reading current state back
to detect drift. Today envs are hand-distributed (`.env` files in
`meerkat/compose/*/`, ad-hoc `pct set` for LXC envs, no audit, no
drift). Orca owns this end-to-end.

**One source of truth.** Env declarations live in the config store
(or a config-repo TOML that reconciles into it). Each declaration:

- A **target selector** — host pattern, CT id, service name, or
  combination.
- A **key** (e.g. `SONARR__AUTH__APIKEY`).
- A **handle** — `op://vault/item/field` for secrets, or an inline
  literal for non-sensitive flags. Plaintext secrets in git are
  banned (CI grep + pre-commit hook).
- A **scope** — secret (no UI display) vs non-secret (visible).
- A **restart policy** — `noop` / `reload` / `restart` on change.

**Per-target adapters.** Same declaration, different mechanism:

| Target | Read current state | Write desired state |
|--------|--------------------|---------------------|
| LXC | `pct exec <id> -- env`, inspect `/etc/environment` + systemd `Environment=` drop-ins + per-service env files | `pct push` `/etc/environment` + templated systemd `EnvironmentFile=`; inner service restart per policy |
| VM | SSH or qemu-guest-agent `exec`; same surfaces as bare metal | Same — SSH/guest-agent write |
| Bare metal | orca-agent reads local files | orca-agent templates writes, commits on success |
| Docker host | `docker inspect` + compose `*.env` on disk | Write `.env` next to compose file; `docker compose up -d` to apply; honor restart policy |
| Orca itself | config store rows | Internal write |

**Drift detection.** Each adapter has a `read_current()` that
returns the live env set on the target. Periodic compare against
the declared set; any diff is a drift event in the lifecycle
timeline (ties into §1.4 of `ROADMAP.md`).

**Secret resolution at projection time.** Handles are resolved on
the orca daemon at projection time, not stored decrypted in the
config store, not handed off to the target until the moment of
write. Adapters receive `(target, key, resolved_value)` over the
mTLS pod channel and must zero the buffer after the write
completes.

**Break-glass.** A target that loses contact with orca falls back
to its last successfully-projected env set on local disk
(encrypted with host key). Targets never call the secret backend
directly.

**Audit.** Every read, write, and projection is logged: actor +
target + key + handle + outcome. **The value is never logged.**

**Rotation propagation — backend-agnostic, user-triggered.** If a
secret is rotated *outside orca* (in 1Password, Bitwarden,
Vaultwarden) or *inside orca* (via the orca-native backend or
`orca secret set`), orca **detects + notifies**, then waits. The
user reviews the affected-consumer list and runs `orca apply
<change-id>` to project. **No automatic re-projection ever** —
this is a hard rule (ROADMAP §1.11).

Only the detection mechanism varies per backend:

| Backend | Detection |
|---------|-----------|
| Orca-native | Internal write triggers projection synchronously; no polling needed. |
| 1Password | Webhook on item update where the deployment exposes one; otherwise periodic version-field poll (item `updated_at` / `version`) per scheduler tick. |
| Bitwarden cloud | API `sync` endpoint returns per-cipher `revisionDate`; poll per tick. |
| Vaultwarden | Same as Bitwarden — same protocol. |

On change detection:

1. `SecretBackend::changed_since(cursor)` returns the set of handles
   that moved.
2. For each changed handle, query the projection graph for every
   `(target, key)` pair that resolves to it.
3. **Emit a pending-change notification** listing affected
   consumers, their restart policies, and a preview of the diff
   (handle moved from version X to version Y; values never shown).
   File a `change_id` in the lifecycle timeline (ROADMAP §1.4)
   with state `pending`.
4. **Wait.** Orca does not resolve, project, or restart anything
   until the user runs `orca apply <change_id>` (or accepts the
   UI prompt). The user may apply selectively per consumer.
5. On apply: re-resolve, re-project, honor each consumer's restart
   policy (`noop` / `reload` / `restart`); flip the lifecycle event
   from `pending` to `applied`. Value never logged.

**Worth noting:** rotation can also originate *inside* orca —
`orca secret rotate <handle>` generates a new value, writes it to
the backend, and follows the same projection path. The change-
detection path above is what catches **out-of-band** rotations
(operator rotated in the 1Password UI directly).

**Detection cadence is configurable per backend.** Webhook-driven
backends are near-instant; polled backends honor a per-backend
interval (default: every scheduler tick — typically 30–60s). A
manual `orca secret refresh` verb forces an immediate sweep.

### 2.3 Unified identity

- The user's orca account is the canonical identity. Service logins
  (SMB on tyr, etc.) are **provisioned from** that identity — same
  username, same password — not minted separately.
- Admin role ⇒ access to everything; don't gate the owner.
- **SMB-on-tyr concretely:** orca provisions a Samba user matching the
  orca username, password sourced from the secret backend; the share
  reconciler ([storage-shares.md](storage-shares.md)) sets `valid users`
  to that identity. Credential also surfaced into the user's Apple
  Keychain on their Mac.

---

## 3. Work breakdown

| # | Item | Size | Notes |
|---|------|------|-------|
| 3.1 | `SecretBackend` trait + registry | M | Handle-prefix routing (`orca://`, `op://`, `bw://`); per-deployment default; per-secret override. |
| 3.2a | Orca-native backend (promote `inline`) | M | Host-identity-derived key, mTLS-gated reads, export/import surface for migration. |
| 3.2b | 1Password backend | L | Connect/service-account token; **personal tenant only** (hard rule); vault scoping. |
| 3.2c | Bitwarden backend | M | API-key auth; collection scoping. |
| 3.2d | Vaultwarden backend | S | Same protocol as Bitwarden; deployment-level URL override. |
| 3.2e | `orca secret migrate --from --to` | M | Walk handles, re-store in new backend, swap prefix in config, re-project. |
| 3.3 | Identity model: orca user → service principals | L | One username; provision SMB/host accounts from it; admin = access-all. |
| 3.4 | Samba-user provisioning on tyr | M | `pdbedit`/`smbpasswd` driven by orca; password from backend; ties to share reconciler. |
| 3.5 | Keychain surfacing (macOS client) | S | Put the unified login into the user's login keychain. |
| 3.6 | Baseline password rotation campaign | L | Rotate root/user/orca on every host; generate unique per-host; store in chosen backend; remove reuse. Parity/rollback per [schema-evolution.md](schema-evolution.md). |

---

## 4. Safety

- Never write plaintext secrets into git-tracked config — only handles.
- Rotation is destructive; stage per-host, verify login before moving
  on, keep a break-glass path (console access) during the campaign.
- mTLS-gated endpoints like the rest of orca.

---

## 5. Cross-refs

- [storage-shares.md](storage-shares.md) — consumes the identity for
  `valid users` / Samba provisioning.
- [pki-lifecycle.md](pki-lifecycle.md) — machine identity (certs) vs
  human identity (this doc).
- [orca-as-logic-layer.md](orca-as-logic-layer.md) — umbrella migration.
- [schema-evolution.md](schema-evolution.md) — parity before rotating
  away from existing credentials.
