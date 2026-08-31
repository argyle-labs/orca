# Build synthesis — shared primitives, gaps, ordering constraints

> Cross-piece distillation of the per-program drills. Sits above
> [`build-order.md`](build-order.md): build-order gives the tier sequence, this
> gives the primitives that recur across tiers, the gaps that block more than one
> program, and the ordering constraints that follow. When this disagrees with
> code or `build-order.md`, those win — fix this file.

## Shared primitives (build once, consumed by many)

Each row is a seam that multiple programs instantiate. Building it once and
landing it early removes duplicate work and prevents divergent copies.

| Primitive | State | Consumers |
|-----------|-------|-----------|
| **N9 `CachedProbe<T>`** — on-demand probe with cached result | absent | inner-service health probes (§1.4), Proxmox poll refactor (§1.1), storage serving runtime health (§1.5) |
| **Converge skeleton** — periodic tick + pure `plan()` + privileged apply | proven in `mount_converge.rs`, not generalized | guest reconciler (§1.1), storage serving (§1.5), drift (§1.3), network/DNS (§1.7), self-heal (§1.9) |
| **Backend trait + registry** — `LazyLock<RwLock<Vec<Arc<dyn>>>>` + JSON-proxy FFI | proven (`StorageBackend`, `SecretsBackend`, `ServiceBackend`) | `AccountBackend` (B1), `AcquisitionSource` (B2) clone the seam |
| **Freshness-SLA + fail-closed** — refuse high-risk action if replica stale | design-only | authz (B1/M2), purchase spend-authority (B2), kill-switch |
| **Monotonic op-log `(stamp_ms, op_id)`** | built (`replication_ops.rs`) | revocation epoch (B1), delete propagation, budget audit |
| **Privileged-op boundary** — `sudo -n orca admin …`, secret-file 0600 | proven (`mount_exec.rs`, `sysadmin.rs`) | storage serving apply (§1.5), `packages` (§1.2), deploy-target provisioning (Tier 5) |
| **Single-writer / `host_owner`** | built (`config_store.rs`) | budget reserve-then-commit (B2), metadata-per-path (B2), AccountBackend owner-node (B1) |
| **Notify dispatcher** — one typed dispatcher, transports as plugins | built (`projects/notifications`, ntfy only) | SMTP transport (Tier 5), media `needs-human`, self-heal, drift events |
| **ConfigSource / meerkat provenance** — GitOps desired-state substrate | in progress (`feat/configsource-mvp`) | every convergence program reads fleet facts from it |
| **Overlap-window rotation** | built (`cert_rotation.rs`) | JWT signing-key rotation (B1/M3) |
| **OpenAPI tool surface** — every `#[orca_tool]` endpoint → OpenAPI 3.1 (`x-codeSamples`, `x-tagGroups`) → generated SDK + Scalar page | built (`serve/mod.rs`, `serve/openapi.rs`, `contract/src/web.rs`) | Peacock (Tier 5) generates one SDK method per endpoint for the app UI **and** renders the Scalar API-docs viewer; it is the UI+docs contract |

## Cross-piece gaps (block more than one program)

Ordered by how much they unblock.

1. **N9 `CachedProbe<T>` is absent.** It is the backbone for §1.4 health probes
   and the Proxmox poll refactor, which in turn gate the §1.1 reconcile gate and
   the §1.2 reboot gate. First substrate item after ConfigSource.
2. **Two disjoint deploy engines.** `projects/deploy-target` is built with **zero
   production callers**; the container reconciler (`containers/src/reconciler.rs`)
   is auto-start/heal-only with no deploy path. The holistic converge engine
   (§A1.1) must unify them before guest/storage/network reconcilers instantiate
   it — otherwise each program forks its own deploy path. Load-bearing risk.
3. **Storage serving side is entirely design-only.** No served-export entity, no
   reconciler, no writer (`/etc/exports`+`exportfs`, `smb.conf`+smbd), no
   advertise, no gateway detect. Only read-side `list_exports` exists. The
   capability seam (core) must land before the nfs/smb/unraid plugins can fill it.
4. **`sudo -n orca admin storage-apply` sudoers grant missing** on non-unRAID
   hosts. Blocks reboot-durable converged mounts (consume side) **and** all
   serving apply. Independent, touches built code (`install.rs`) — can land
   first, unblocks the live fleet immediately.
5. **`mount_converge` render gap unroot-caused.** A correctly-enabled placement
   returned `changed: []` (silent skip in `desired_for_host` join filters). Must
   be root-caused before either consume or serving convergence is trusted on the
   fleet.
6. **PBS restore is a stub** (`PbsMethod::restore` errors) and there is no
   restore-drill harness. Backup (§1.6) is not verifiable end-to-end until both
   land.
7. **SMTP transport not built.** Notify has ntfy only. The transport is a plugin
   (no core dispatcher change) and ships now; per-user recipient resolution needs
   the B1 identity directory.
8. **Stable FQDN over Tailscale not fixed.** Hard prerequisite for the first
   passkey (WebAuthn RP-ID) and OIDC redirect URIs (B1/M4).
9. **Tier-5 drivers greenfield.** `sync-callers.yml`, Gitea cargo-registry, and
   the channel-based fleet-roll driver do not exist; the rest of CI/CD is built.

## Ordering constraints (the "no gaps" spine)

Beyond the two governing rules (parity gates service surface; core seams before
plugin producers), these cross-piece constraints fix sequence:

- **ConfigSource → N9 → §1.4 health → {§1.1 reconcile gate, §1.2 reboot gate}.**
  Health probes are the shared prerequisite for both gates; N9 is their backbone.
- **Deploy-engine unification (§A1.1) before any reconciler instantiation.** Guest
  (§1.1), storage serving (§1.5), and network (§1.7) reconcilers are all
  instantiations of the unified converge surface.
- **Sudoers grant (gap 4) before storage serving apply (§1.5).** Serving writes
  need the same privilege boundary; ship the grant first.
- **Storage capability seam (core) before nfs/smb/unraid serving plugins.**
- **B1/M1 groups + authz before media add-request approval (B2 step 2).** Media
  Milestone A (native tail) ships **before** B1; only Milestone B (approval +
  purchase) gates on B1/M1 + M2.
- **FQDN fixed before B1/M4 passkeys.**
- **Tier-5 CI/CD lands early** — it releases every other program.
- **Peacock tracks the tool surface, not a phase.** The orca-side web seam is
  built; peacock serves both the app UI (generated SDK) and the Scalar API-docs
  viewer from the OpenAPI surface, so a read-oriented dashboard **and** live API
  docs are buildable now and each program's new `#[orca_tool]` endpoints extend
  both automatically. Its one hard edge is auth: sign-in migrates to
  mesh-signed-JWT at B1/M3, federated/passkey pages at B1/M4, group-gated nav at
  B1/M1, media views at B2.

## Lane assignment (concurrent after Tier 0)

- **Lane A — Platform:** ConfigSource → N9 + converge unification → §1.4 → §1.1 →
  §1.5 (+ gaps 3,4,5) → §1.2 → §1.3 → §1.6 (+ gap 6) → §1.7 → §1.8, with §1.9
  self-heal woven across plugins.
- **Lane B — Identity → Media:** B1/M1–M6 (gap 8 first) → media Milestone A
  (native tail, no B1 dependency) → media Milestone B (approval + purchase, on
  B1/M1+M2).
- **Tier 5 — cross-cutting:** CI/CD finish, cargo-registry, fleet-roll, registry
  discovery, provisioning, SMTP transport (gap 7), Peacock web UI (tracks the tool
  surface throughout). Feeds both lanes.
