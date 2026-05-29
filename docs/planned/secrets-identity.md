# Secrets + unified identity — scope

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

A `SecretBackend` trait so orca can resolve/store secrets from a
configured provider instead of only `inline`:

```
SecretBackend (trait)
  ├── inline      (exists today — encrypted-at-rest in orca store)
  ├── onepassword (FIRST — via op CLI / Connect; vault+item refs)
  ├── bitwarden   (later)
  └── vaultwarden (later — self-hosted bitwarden API)
```

- Secrets are referenced by handle (`op://vault/item/field` style for
  1Password), never inlined into config repos.
- Session handling: the 1Password desktop integration expires fast;
  orca should use a **Connect server / service-account token** for
  unattended resolution rather than the interactive desktop session.
- **Tenant separation (hard rule):** personal/homelab secrets use the
  user's **personal** 1Password account only — never the rebuy *work*
  tenant that the existing `rebuy auth op` CLI authenticates against.
  Orca's 1Password backend must be configured with a personal-account
  service-account token, fully separate from the rebuy integration.
- `system_secret_*` endpoints gain backend selection
  (`system_secret_backends` already lists kinds; today only `inline`).

### 2.2 Unified identity

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
| 3.1 | `SecretBackend` trait + registry | M | Generalize beyond `inline`; handle-based refs. |
| 3.2 | 1Password backend (Connect/service-account) | L | Unattended token, not the 10-min desktop session; vault scoping. |
| 3.3 | Identity model: orca user → service principals | L | One username; provision SMB/host accounts from it; admin = access-all. |
| 3.4 | Samba-user provisioning on tyr | M | `pdbedit`/`smbpasswd` driven by orca; password from backend; ties to share reconciler. |
| 3.5 | Keychain surfacing (macOS client) | S | Put the unified login into the user's login keychain. |
| 3.6 | Baseline password rotation campaign | L | Rotate root/user/orca on every host; generate unique per-host; store in 1Password; remove reuse. Parity/rollback per [schema-evolution.md](schema-evolution.md). |
| 3.7 | Bitwarden / Vaultwarden backends | M | After 1Password proves the abstraction. |

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
