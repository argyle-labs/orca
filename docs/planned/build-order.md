# Build order — consolidated, dependency-ordered

> Forward-looking sequencing across the whole program set. Grounds the phased
> roadmap in [`README.md`](README.md), the design records in this directory, the
> as-built subsystems in `docs/`, and the design doc
> [`../design/unified-media-acquisition.md`](../design/unified-media-acquisition.md).
> When this disagrees with code or `README.md`, those win — fix this file.

## Two governing rules (from `README.md`)

- **Parity rule.** Phase 1 (system-lifecycle parity) gates Phase 2 (service
  surface). Nothing retires a hand-maintained host script until it passes the
  four-check parity test on every target host.
- **Golden ordering rule.** Core (orca) seams land **before** plugin producers,
  in *every* program (deploy, DNS, self-heal, CI, identity, media).

## Execution model: two parallel lanes

After the operational front (Tier 0) is stable, two lanes run concurrently, each
obeying core-before-plugins:

- **Lane A — Platform:** Tier 1 substrate → Tier 2 lifecycle parity.
- **Lane B — Identity → Media:** Tier 3 identity/RBAC → Tier 4 media.

Lane B does not depend on Lane A's converge engine and does not wait behind
parity. Tier 5 (CI/CD + release + registry) is cross-cutting and feeds both.

---

## Tier 0 — Operational front (precedes feature work)

Sequence:

1. **Memory-leak verdict** — run the clean rc.12 RSS window, analyze
   (`scratchpad/sample_rss.sh` + `analyze.py`; 2nd-half slope >8 MB/h
   non-decelerating = leak); heap-profile (Linux) if it climbs.
2. **Self-heal live-validate** — reboot willow, confirm baldur `/mnt/data`
   auto-heals with zero manual intervention.
3. **Coverage → 90%** — HARD standing directive; behavioral tests only, **do not
   touch the gate or `.coverage-floor`**; one workflow at a time.
4. Housekeeping: `peer_secure=False` mesh-PKI trust gaps (baldur/frigg/freyr/
   maple); delete `.bak-rc8-preswap` binaries; create missing
   `.orca/{logs/sessions,memory}` dirs.

---

## Lane A — Platform

### Tier A1 — Substrate (core seams everything instantiates)

1. **Holistic deploy/converge engine** — the reconcile engine. The guest
   reconciler, storage-serving, and network reconciler are *instantiations* of
   its surface / deployable / placement / converge contracts. Design + slices
   1–2 merged (routes model, replication-generic + failover gate). **RESUME at
   slice 3:** surface substrate on willow+maple (advertise unraid+docker+dockge,
   adopt existing) → syncthing adopt-or-deploy feeding the failover gate →
   generalize converge to container/stack/LXC/VM (spec now, build last). See
   [`holistic-deploy-converge.md`](holistic-deploy-converge.md).
2. **Generics backbone** — land **N9 `CachedProbe<T>`** (the on-demand-probe
   backbone consumed by health probes A2.1 and the proxmox poll refactor) and
   **NEW-I1 `#[service_backend]`** macro (~20-plugin multiplier). Remainder of
   the punchlist lands opportunistically. See
   [`plugin-generics-punchlist.md`](plugin-generics-punchlist.md).
3. **ConfigSource / meerkat declarative config** — the GitOps desired-state
   substrate (repo→reconcile / orca→PR-writeback) every convergence program reads
   from. Directive: *check meerkat for fleet facts* rather than re-probing hosts.
   Schema runtime-generated, never committed; repo holds DATA only.

### Tier A2 — Lifecycle parity (Phase 1), on A1

Internal order by dependency:

1. **§1.4 inner-service health probes** *first* (uses N9) — prereq for both the
   reconcile gate (§1.1) and the reboot gate (§1.2).
2. **§1.1 Proxmox guest reconciler** — instantiate the converge
   engine for LXC/VM: pct.conf/qemu-server.conf parse+diff+apply, per-key
   strategy registry, bind-readiness probe gating `pct start`, inner-service
   health gate, `{reconcile,drift,restore}` unit actions.
3. **§1.5 storage serving side** — NFS export + SMB share reconcilers, Avahi/WSD
   advertise, gateway-mode detect, runtime health + failover across a share's
   ordered sources. Clears the acute
   [`storage-serving-followups.md`](storage-serving-followups.md) items (SMB-user
   reboot durability; daemon mount privilege).
4. **§1.2 host update lifecycle + `packages` primitive** — dnf/pacman/pkg/opkg
   drivers, `updates.toml`, GPU/accelerator DKMS **rebuild + verify-load** after
   kernel upgrade, reboot orchestration (ordered pre-hooks, health-gated
   rolling). `packages` is a standalone new crate
   ([`packages-primitive.md`](packages-primitive.md)); NVIDIA first.
5. **§1.3 fleet-wide drift detection** — per-noun drift-checker registrations,
   drift-event schema, version-drift notify (host lagging its channel).
6. **§1.6 backup** — minimal-backup step 1c (Docker/service `BackupSpec`) + PBS
   plugin + restore-drill harness ([`minimal-backup-rfc.md`](minimal-backup-rfc.md);
   as-built on the dedicated `backup` domain, `../BACKUP-SUBSYSTEM.md`).
7. **§1.7 network reconciler + DNS convergence self-heal** — `resolution-target`
   crate mirroring deploy-target; `dns_converge.rs` pure `plan()`; producers for
   OPNsense Unbound then AdGuard; split-DNS multi-resolver. Incident-driven
   (gitea.scottkey.me NXDOMAIN negative-cache).
8. **§1.8 host lifecycle** — `system.doctor`, `system.uninstall`, decommission
   (drain → deregister → wipe secrets/certs).
9. **Plugin self-heal + correlation** (directive, woven across plugins as they're
   touched) — every plugin Detect/Notify/Offer-fix/Point-to-root-cause via
   Check→Diagnosis→Remediation; core correlates cross-plugin signals into ONE
   incident (host→service→mount) + a recurrence engine escalating chronic issues.

---

## Lane B — Identity → Media

### Tier B1 — Identity / Auth subsystem (greenfield; gate for all multi-user features)

The prerequisite the media adversarial review surfaced (#279/#280 — **design doc
pending before code**). Independent of Lane A's converge engine. Build order:

1. `principal` + `groups` table (uuid-referenced) + **AccountBackend** trait +
   registry (mirror `StorageBackend`); seed default paired groups (media/
   media-admins, automation/automation-admins, networking/networking-admins)
   **only on the first node of a fleet** (joining nodes inherit via replication —
   never local-seed, or the mesh resurrects deleted groups).
2. **Freshness-SLA + fail-closed** authz for security-critical state (mesh is
   wall-clock LWW, no central store): a node refuses high-risk actions unless its
   replica synced within N seconds. Monotonic revocation epoch (reject lower).
   Replication-layer protected-key guard for the undeletable root admin grant +
   loopback break-glass.
3. **forward_auth (Phase-1 verify)** — mesh-signed JWT, `/auth/verify`,
   fail-closed at Caddy, load-balanced across multiple orca nodes; silent-refresh
   so short TTLs don't interrupt in-flight streams.
   - **Federated login + multi-method account linking.** orca is also an
     external-IdP **relying-party**: local username/password, passkeys, and
     "Sign in with **Google** / **Discord**" (OIDC/OAuth2 RP), with **multiple
     methods bound to one principal** (an `identity` table of `(provider,
     external-subject)` rows). Enabled by a stable **FQDN over Tailscale**
     (one origin for redirect URIs + webauthn RP-ID). Linking is
     security-critical: re-auth with a trusted method before linking, **no
     implicit email auto-linking**, unlinking can't orphan the account, and
     external IdP compromise is a stated residual risk (high-risk actions may
     still demand re-auth). See [`../design/unified-media-acquisition.md`] and
     the pending identity design doc.
4. **AccountBackend projection** — event-driven reconcile + periodic backstop,
   single owner-node per (service, account); two tracks (delegate_oidc /
   manage_account+set_credential). Per-device app-passwords.
5. **OIDC provider (Phase-2)** + **per-user credential brokerage** (retrieve own
   creds for enabled apps) + fixed **webauthn RP-ID strategy** for multi-origin
   (LAN / tailscale / public). See
   [`../design/unified-media-acquisition.md`](../design/unified-media-acquisition.md)
   "Identity, SSO, and credential brokerage".

### Tier B2 — Media platform (Phase 2), on B1 + A1

Design of record: [`../design/unified-media-acquisition.md`](../design/unified-media-acquisition.md).

1. **`AcquisitionSource` trait + registry** in core (the media capability seam;
   registered in `../CAPABILITY-REGISTRIES.md`).
2. **Add-request → approval front door (Overseerr model)** — user requests an
   add; a `media-admins` member approves; orca creates the monitored item via the
   *arr add-API; the *arr then auto-download as normal (their grab is NOT gated —
   they keep full autonomy). Requires B1 groups/authz.
3. **Native purchase/rip providers** (second tail) — start with the *stable five*:
   Standard Ebooks (OPDS), DriveThruComics (API), itch.io (API), Internet Archive
   (CLI), Libro.fm; then Bandcamp/Humble (fragile, best-effort). **Native-DRM-free
   only** — ship/orchestrate no de-DRM tooling.
4. **Purchase spend-authority** — owner-node single-writer budget
   (reserve-then-commit), per-grantee aggregate ceiling, idempotency + rate limit,
   store-authoritative price pinning, out-of-band confirm to the card owner,
   hash-chained audit, kill-switch via freshness-SLA. Requires B1.
5. **Metadata/management** — single-writer-per-path via staging handoff;
   provider-aware identity; `_unmatched/<source>/` dead-letter; per-backend
   scan-on-import adapters (contention-avoidance, not retry, for the Lidarr wedge).

---

## Tier 5 — Cross-cutting enablers (feed both lanes; not a late phase)

- **Unified CI/CD pipeline** — ACTIVE; finish orca RC landing on the shared
  Gitea-canonical basis, then roll to plugins via `sync-callers.yml` and retire
  `plugin-ci`/`plugin-release`. Releases everything else, so land early.
- **Cross-repo release orchestration** + **Gitea cargo-registry migration**
  (version-pinnable builds) + channel-based fleet-roll (rc→beta, stable→both;
  fleet update pulls PUBLIC GitHub assets).
- **Plugin registry discovery** — release-derived availability + org-topic
  membership + 2FA-gated third-party manifests (retires the hand-curated
  `plugin_catalog.json` status). See
  [`plugin-registry-discovery.md`](plugin-registry-discovery.md).
- **Deploy-target provisioning (opt-in)** — any host opts in to become a deploy
  target; generic Linux hosts get a provisioned docker/podman runtime via one
  scoped sudoers bootstrap. First target = hemlock.
- **Notification transports** — extend the shipped `notify.send` typed dispatcher
  (`projects/notifications`) with pluggable delivery channels beyond ntfy:
  **email (SMTP) now** — reliable, per-user address from the identity directory,
  templated typed events; **push later** — APNs/FCM, gated on the native app
  (deferred). Delivery is a transport plug-in behind the one dispatcher; the
  media `needs-human` / self-heal / drift events are all consumers.

---

## Critical path to the media / per-user features

`Tier 0 (ops) → Tier B1 (identity/RBAC) → Tier B2 (media)`, running in Lane B
parallel to Lane A. The identity subsystem (B1) is a prerequisite: it is designed
(design doc pending) and built before any add-request approval, purchase
capability, or credential brokerage.
