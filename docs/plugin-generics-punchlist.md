# Plugin → Core Generics Punchlist

> **Purpose:** cross-plugin survey (2026-07-27) of logic reimplemented per-plugin that should be
> abstracted UP into orca core (`contract` / `utils` / `plugin-toolkit` / the `#[endpoint_resource]`
> derive / the shared backend traits). A todo backlog for other sessions — findings only, nothing
> here is implemented yet.
>
> **How this was produced:** read-only survey of every substantive plugin
> (proxmox, unraid, dockge, docker, nfs, smb, s3, walrus, plex, jellyfin, arr, raccoon, mcp, nut,
> ntfy, homeassistant, agents, gitea) against a baseline inventory of what `plugin-toolkit`/core
> *already* provide, so nothing already-generic is re-proposed.
>
<!-- WS2 dependency-graph finding (2026-07-27, verified on rc.47): the plan's stated home
`projects/utils` is CYCLE-BROKEN as-is — `utils` optionally depends on `contract` (single call
site: `utils/src/state/mod.rs:44` → `contract::config::orca_home()`, gated by the `state` feature).
`contract` is the true dependency-free base. To make `utils` the shared leaf the plan wants (so
`contract`/`db`/`pod` AND light/storage-only plugins can all share ONE `Route`), first SEVER that
edge: inline `orca_home()` (trivially `$ORCA_HOME || $HOME/.orca`) into `utils::state` and drop the
optional `contract` dep from `utils`. Then `Route` lives in `utils` (ungated, like `utils::{hash,id,url}`
which plugin-toolkit already re-exports), and `contract` gains a `utils` dep to host `ClaimAddress` as
`Route`. Alternative rejected: hosting in `contract` would exclude light plugins (contract is an
optional, `tools`-gated dep of plugin-toolkit). -->

> **Related:** `~/.claude/plans/noble-rolling-seal.md` (WS2 = the `Route`/`routes[]` migration),
> and the architecture rules in memory: `orca-core-generic-plugins-expose-functionality`,
> `no-top-level-urls-use-addresses-array`, `on-demand-not-poll-and-cache`,
> `plugins-test-all-functionality-incl-backups`, `apis-small-scoped-lean-no-kitchen-sink`.

Effort key: **S** ≈ ½–1 day · **M** ≈ 2–4 days · **L** ≈ multi-day / RFC + phased rollout.

---

## Section 0 — Baseline: what plugins ALREADY get for free (do NOT re-propose)

These exist today; several punchlist items are just "adopt the existing thing."

- **`#[endpoint_resource]`** (`projects/derive/src/endpoint_resource.rs`) — generates the endpoint row
  struct, `EndpointEntry` (secret-filtered), `endpoint_db::{list,get,require,insert,update,upsert,remove}`,
  the 5 CRUD `#[orca_tool]`s, SQL schema, **and a built-in `routes: Vec<Route>` column + `enabled`**
  (the column/type is `addresses`/`Address` in current code; WS2 renames it to `routes`/`Route` — see N3).
- **`address::resolve_reachable()` + last-good cache** (`plugin-toolkit/src/address.rs`) — ordered
  multi-path fallback over `routes[]` with per-endpoint caching (module/fn renames to `route::…` in WS2).
- **HTTP client** (`utils/src/http`) — pooled, TLS, timeouts, insecure mode, `HttpError`.
- **Secrets** (`plugin-toolkit/src/secrets.rs`) — `SecretRef`, `get_required`, `scoped_name`.
- **Error taxonomy** — `ErrorKind`→HTTP/CLI-exit (`contract/src/error.rs`) **and the `#[orca_error]`
  derive** (currently named `#[plugin_error]`, `derive/src/plugin_error.rs` — **rename to `orca_error`**)
  that injects `Display`/`Error`/`From`.
- **Topology** — `TopologyClaim`/`ClaimAddress`/`ClaimEndpoint` + uuidv7 identity/merge
  (`contract/src/topology.rs`), `#[derive(Replicated)]` natural-key merge (`derive/src/lib.rs`).
- **Backup contract** — `BackupSpec`/`BackupStrategy`/`Retention`/`BackupRef` (`contract/src/backup.rs`).
- **Notifications** — `Event`/`EventClass`/`Severity`/`Dispatcher`/`Backend` (`notifications/src/lib.rs`).
- **StorageBackend trait** + `probe_health`/`mount_table`/`MountStyle`/`SmbCredentials`
  (`storage/src/lib.rs`); **DeployTarget** and **ServiceBackend** traits likewise.
- **Process** (`plugin-toolkit/src/process.rs`), **time** (`now_millis_since_epoch`, `compact`),
  descriptor/openapi/graphql codegen, the `prelude`.
- **Confirmed NOT yet generic** (baseline §23): **pagination**, **retry/backoff**, **TTL/cache invalidation**.

---

## Section 1 — Adoption gaps (already generic, just not used) — cheapest wins

| # | Item | Plugins not adopting | Fix | Effort |
|---|------|----------------------|-----|--------|
| A1 | **Hand-rolled `Display`/`Error`/`From` error enums** instead of the `#[orca_error]` derive (currently `#[plugin_error]` — rename it) | plex `lib.rs:56-66`, jellyfin `lib.rs:66-97`, arr-base `lib.rs:90-121`, ntfy `lib.rs:49-73`, homeassistant `lib.rs:32-62` | Rename the derive `plugin_error`→`orca_error`, then switch each error enum to `#[orca_error]` (already injects the boilerplate + `From<HttpError>`). If the derive genuinely can't route `HttpError`, that's a one-time toolkit fix, then adopt everywhere. | S |
| A2 | **Scalar `base_url` instead of the built-in `routes[]` + `resolve_reachable`** | plex `tools.rs:46-53`, jellyfin `tools.rs:47-54`, ntfy `lib.rs:206-212`, homeassistant `tools.rs:36-43` (gitea/proxmox/arr already do it right) | Populate the macro's `routes` column, call `resolve_reachable()` in `make_client`. Folds into **N1/WS2**. | S each |
| A3 | **Custom notify emit** vs the `notifications` dispatcher | plex has a `// TODO notify` (`account/mod.rs:69-86`); jellyfin already uses `plugin_toolkit::notify::emit` correctly (`tools.rs:407-448`) | Make jellyfin's pattern idiomatic; wire plex's TODO to the existing dispatcher. | S |

---

## Section 2 — New abstractions, by proposed home

### `#[endpoint_resource]` derive enhancements

| # | Item | Duplicated in | Proposal | Effort | Depends on |
|---|------|---------------|----------|--------|-----------|
| N1 | **`make_client()` / endpoint-resolution preamble** — `endpoint_db::get/require` → check `enabled` → `resolve_reachable` → build `Config` → `Client`, hand-written in *every* HTTP plugin | proxmox `tools.rs:69-81`, unraid `tools.rs:24-57`, dockge `tools.rs:43-50`, plex `tools.rs:46-53`, jellyfin `tools.rs:47-54`, arr sonarr `lib.rs:32-46`, gitea `tools.rs:55`, ntfy, homeassistant | Have the macro emit a `resolve_endpoint_url(name)` (row-fetch + enabled-check + `resolve_reachable`) and optionally a `make_client`. Highest ROI item in this doc — touches ~10 plugins. | M | N3 (Route) helps but not required |
| N2 | **`for_each_enabled_endpoint()` fan-out with per-endpoint warn+skip** — proxmox already has the clean version; others inline it | proxmox `tools.rs:110-140` (reference), unraid `topology.rs:24-41`, dockge `unit_provider.rs:83-107` | Lift proxmox's impl into `plugin_toolkit::prelude`; others import it. | S | — |

### `Route` / `routes[]` (WS2 — see plan `noble-rolling-seal.md`)

| # | Item | Duplicated in | Proposal | Effort | Depends on |
|---|------|---------------|----------|--------|-----------|
| N3 | **Scalar address fields** (`base_url`/`host`/`url`/`socketPath`/`endpoint`/`source`+`failover_sources`) instead of the unified ordered `routes[]` of `Route`| unraid `endpoint.rs`, dockge `tools.rs:31-39`+`topology.rs:84-113`, docker `engine.rs:27-33`, s3 `lib.rs:122`, plex `lib.rs:42-54`, jellyfin `lib.rs:51-64`, arr-base `lib.rs:69-84`, ntfy `lib.rs:28-46`, homeassistant `lib.rs:14-26`, gitea `lib.rs:44-89`, nfs/smb mount sources | **This is WS2 (IN PROGRESS — worktree `feat/routes-ws2`).** ⚠️ The plan listed only docker/dockge/unraid — the smell is **fleet-wide** (11+ plugins); every one migrates. Retire `runs_on_from_base_url()` URL-parsing (dockge `topology.rs:88-113`) for a `Route`-native host accessor. **CLEAN BREAK — no back-compat** (operator 2026-07-27): no `#[serde(alias)]`, no dual-shape tolerance, no column read-migration; fix every call site universally and let stored rows re-materialize. | L | NFS extraction landed in rc.47 ✓ |

### `plugin-toolkit` — HTTP / client

| # | Item | Duplicated in | Proposal | Effort |
|---|------|---------------|----------|--------|
| N4 | **`Client { cfg, http }` wrapper** (`new()`/`with_http()`/`authed_get()`) reimplemented per plugin | plex `lib.rs:147-158`, jellyfin `lib.rs:140-156`, arr-base `lib.rs:228-244` | `GenericApiClient<C: ApiConfig>` or a derive that generates the wrapper. | S |
| N5 | **Auth-header construction** — each plugin hardcodes its scheme | plex `x-plex-token` `lib.rs:224-230`, jellyfin `MediaBrowser Token` `lib.rs:224-230`, arr-base `x-api-key` `lib.rs:287-294`, proxmox `PVEAPIToken` `lib.rs:113-126`, unraid `x-api-key`, gitea bearer | `AuthScheme` trait with impls (`XApiKey`, `Bearer`, `XPlexToken`, `MediaBrowserToken`, `PveApiToken`); `ApiClientBuilder` takes a `HeaderFactory`. Retires dockge's hardcoded `ws://` too. | S |

### `plugin-toolkit` — process / backup

| # | Item | Duplicated in | Proposal | Effort |
|---|------|---------------|----------|--------|
| N6 | **Subprocess run + exit-code→error + stderr capture** — `Command::output()` → non-zero → `"command failed (exit {code}): {stderr}"` | ntfy `lifecycle.rs:59-74`, homeassistant `lifecycle.rs:82-97`, nfs `lib.rs:411-479`, smb `lib.rs:221-232`, s3 `lib.rs:261-346`, walrus `checks.rs:716-730` | `plugin_toolkit::process::run_checked()` wrapper over the existing `Command` — captures streams, classifies exit, optional env-secret passing (s3's `AWS_*` pattern). | S–M |
| N7 | **Backup/restore tar scaffolding** — tar.gz + timestamp naming + validate-source + mkdir-dest, per plugin (the *contract* exists, the *executor* doesn't) | ntfy `lifecycle.rs:244-331`, homeassistant `lifecycle.rs:331-421` (with `--exclude`), plex `lifecycle.rs:80-200`, jellyfin `lifecycle.rs` | `plugin_toolkit::backup::{backup_tar, restore_tar}` (with excludes) building on `BackupSpec`/`BackupRef`. Directly supports the "test all functionality incl. backups" rule. Also dedups `now_stamp()` (ntfy `lifecycle.rs:335`, ha `lifecycle.rs:425`) → use `time::compact()`. | M |

### `plugin-toolkit` — health / caching (on-demand model)

| # | Item | Duplicated in | Proposal | Effort |
|---|------|---------------|----------|--------|
| N8 | **HTTP liveness / health probe** with timeout, result as reachable+detail | plex `lib.rs:220-222`, jellyfin `lib.rs:187-189`, mcp `lifecycle.rs:66-104`, nut `lib.rs:143-190` | `HealthProbe` trait / `probe_with_timeout()` in toolkit (StorageBackend already has `probe_health`; generalize to HTTP/socket services). | S–M |
| N9 | **Lazy-cache + TTL + force-refresh** — baseline confirms this is NOT formalized; every backend re-probes on each call | nfs `lib.rs:384-399`, smb `lib.rs:560-601`; the whole proxmox 18s poll problem | Generic `CachedProbe<T>` / TTL cache keyed by (entity, datum) with force-refresh. **This is the core primitive behind `on-demand-not-poll-and-cache`** — build it here, then proxmox/others consume it. | M |
| N10 | **Concurrent batch-probe** (`join_all` over items + classify) | nfs `lib.rs:384-399,831-900`, smb `lib.rs:560-601` (currently sequential — could parallelize) | `BatchProbe`/`concurrent_map` helper (items, probe fn, classifier, timeout budget). | M |

### `plugin-toolkit` / core — registration boilerplate

| # | Item | Duplicated in | Proposal | Effort |
|---|------|---------------|----------|--------|
| N11 | **`backend_dispatch` prefix-router** — repeated `strip_prefix(PREFIX).and_then('.')` chains | proxmox `registration.rs:68-82`, unraid `registration.rs:61-88`, dockge `registration.rs:55-72`, nut `registration.rs:18-68`, agents `registration.rs:48-73` | Macro/builder generating the router from `(prefix, handler)` pairs. | S |
| N12 | **`diagnostics_backend_def()` helper** — mirrors existing `topology_backend_def`/`unit_backend_def` | proxmox `diagnostics.rs:40-46`, unraid `registration.rs:40-44` | Add the helper alongside the existing ones. | S |
| N13 | **Topology dispatch→JSON wrapper** — `match op { "collect_claims" => block_on + to_string }` | proxmox `topology.rs:30-36`, unraid `registration.rs:91-100`, dockge `registration.rs:75-85` | `plugin_toolkit::export::dispatch_topology_op()`. | S |
| N14 | **`TopologyClaim` builder** — identical struct literals | proxmox `topology.rs:97-115`, unraid `topology.rs:50-58`, dockge `topology.rs:59-72` | Fluent `TopologyClaimBuilder::new(provider)…build()`. Convenience only — low priority. | S |

### `plugin-toolkit` / core — misc

| # | Item | Duplicated in | Proposal | Effort |
|---|------|---------------|----------|--------|
| N15 | **Retry/timeout combinators** — timeouts hardcoded as module consts, used with `tokio::time::timeout` | proxmox `containers_adapter.rs`/`unit_provider.rs:45-46`, dockge `lib.rs:36-40`, nut, mcp | `plugin_toolkit::utils::{retry, with_timeout}` combinators (config/env-driven defaults). Pairs with the "not-yet-generic retry/backoff" baseline gap and the pod-mesh backoff primitives. | M |

### Naming — retire the `plugin_` prefix on core derives (operator: "all `plugin_` needs to go away")

The derive crate already names most macros `orca_*` / `endpoint_*` (`orca_async`, `orca_tool`, `endpoint_tool`, `endpoint_resource`). Two stragglers still carry a `plugin_` prefix even though they're generic orca-core machinery, not plugin-subsystem concepts.

| # | Item | Where | Proposal | Effort |
|---|------|-------|----------|--------|
| N22 | **Rename the `plugin_`-prefixed derives → `orca_`** | `derive/src/lib.rs:141` `plugin_struct` (25 usages), `:153` `plugin_error` (6 usages); shared field-attr `#[plugin(display=…, from, rename_all=…, skip_if_none)]` | Rename proc-macros `plugin_struct`→`orca_struct`, `plugin_error`→`orca_error`; rename helper attr `#[plugin(...)]`→`#[orca(...)]`. Update the `prelude` re-exports and all call sites. Mechanical but touches every plugin repo (git-dep of `derive` via toolkit) → coordinate a toolkit release + fleet reinstall like any breaking change. Subsumes the rename half of **A1**. | S (in-repo) / M (cross-repo rollout) |

> **Scope decision needed — the `plugin_toolkit` crate itself.** "All `plugin_` gone" could extend to
> renaming the `plugin_toolkit` crate → `orca_toolkit` (64+ in-repo files + the `branch=main` git-dep in
> every external plugin repo + every `use plugin_toolkit::…`). That's a large coordinated rename, distinct
> from the derive cleanup above. Likewise the plugin-management **domain** (`plugin.list`/`install`/`invoke`,
> `plugin-toolkit-build`) is legitimately *about* the plugin subsystem, not a mislabeled core primitive —
> probably keep. Confirm whether the crate rename is in scope before doing N22's cross-repo sweep so both
> land in one release.

### Storage-backend specifics — **coordinate with the in-flight NFS core→plugin extraction**

| # | Item | Duplicated in | Proposal | Effort |
|---|------|---------------|----------|--------|
| N16 | **Option-grammar parse/render** — split comma string → typed model (bounds-checked) → re-render, structurally identical per backend | nfs `lib.rs:1379-1548`, smb `lib.rs:334-485`, s3 `lib.rs:154-226` | `OptionParser<T>` trait + bounds-check helpers; each backend keeps only its grammar. | M |
| N17 | **Safety-floor injection** — "if key X absent, add default Y" loops (nfs soft/softreval/timeo/retrans; smb creds-file; s3 no-secret) | nfs `lib.rs:1528-1548`, smb `lib.rs:458-485,686-703` | `SafetyFloor` trait/derive declaring per-option defaults + a compile-time SecretRef-never-rendered guard. | M |
| N18 | **StorageBackend `name()`/`kind()`/`endpoint()` boilerplate** | nfs `lib.rs:1584-1607`, smb `lib.rs:632-656`, s3 `lib.rs:350-374` | `#[storage_backend(name=…, kind=…, endpoint=…)]` attribute macro. | S |
| N19 | **Backend error→StorageError/AdapterError mapping** (`ToolFailed`/`MissingTool`/`Io` variants + status-code dispatch) | nfs `lib.rs:25-38`, smb `lib.rs:39-54`, s3 `lib.rs:50-63`, docker `runtime_adapter.rs:256-283` | `#[backend_error]` macro with configurable status→variant mapping. | M |

### New shared contract — media

| # | Item | Duplicated in | Proposal | Effort |
|---|------|---------------|----------|--------|
| N20 | **`SessionTranscodeHealth`** — same struct name + core flags (`is_transcoding`, `software_fallback`, hw state) normalized from different wire formats | plex `diag.rs:120-225`, jellyfin `diag.rs:60-145` | Canonical struct + `TranscodeClassifier<Raw>` trait in a shared media contract (`contract/src/media.rs` or new `media-contract` crate). First concrete step toward the `media-plugins-expose-titles-to-mesh` directive. | M |
| N21 | **Storage-drift detect/remediate scaffolding** — DETECT (read-only) + REMEDIATE (dry-run default, `--apply`) with `StorageIssue`/report types | plex `storage.rs:42-150` (DB rewrite), jellyfin `storage.rs:40-100` (API rescan) | `StorageRemediationScaffold` trait (scaffold only; remediation stays plugin-specific). | S |

---

## Recommended sequencing

1. **Adoption gaps A1–A3** (S each) — pure cleanup, no new API, immediate consistency.
2. **N1 `make_client` generation** + **N2 fan-out** + **N4/N5 client+auth** — the endpoint/HTTP cluster,
   ~10 plugins, mostly S–M, no fleet migration.
3. **N9 TTL cache** + **N8 health probe** + **N10 batch probe** — build the primitives the
   `on-demand-not-poll-and-cache` north-star needs *before* refactoring proxmox's 18s poll.
4. **N6 run_checked** + **N7 backup tar** — process/backup helpers; N7 directly enables the
   "test-all-incl-backups" rule.
5. **N11–N15** registration/retry boilerplate — S-heavy, low-risk.
6. **N3 routes[]** — this is WS2; **blocked on NFS extraction landing on `main`**. Note the expanded
   plugin list above when WS2 starts.
7. **N16–N19** storage-backend specifics — **must coordinate with the in-flight NFS worktree** (it may
   already be relocating some of this).
8. **N20/N21** media contract — larger design, ties to the media-mesh directive.
9. **N22** `plugin_`→`orca_` derive rename — bundle into the next breaking toolkit release (WS2 is a
   natural carrier since it already forces a fleet-wide plugin rebuild); resolve the `plugin_toolkit`
   crate-rename scope question first so it lands in the same release.

## Cross-cutting notes

- **NFS is being refactored in another worktree** (core→plugin extraction). Anything touching
  nfs/smb/s3 option grammar, safety floors, or backend traits (N3 sources, N16–N19) must be
  reconciled with that work before starting.
- Every new generic must respect `apis-small-scoped-lean-no-kitchen-sink` (don't fold fat/optional
  data into these helpers) and `orca-core-generic-plugins-expose-functionality` (core stays generic;
  no domain grammar leaks back into core — the very rule WS1 is enforcing for NFS).
</content>
</invoke>
