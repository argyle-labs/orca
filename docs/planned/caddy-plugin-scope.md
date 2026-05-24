# Caddy plugin + meerkat-as-config — scope

Goal: stop hand-editing `compose/caddy/Caddyfile`. Make orca own Caddy's
config and certs on baldur, with route definitions living in meerkat as
config-as-code. Any paired orca peer can CRUD routes via MCP/REST; baldur
reconciles the Caddyfile and reloads.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Why

- Today: single hand-written Caddyfile, one upstream per vhost, no
  fallbacks, no IPv6, no programmatic mutation.
- `orca.scottkey.me` needs round-robin across reachable orca peers, but
  orca's `:12002` requires client-cert mTLS — Caddy can't reach it
  without an issued peer cert.
- We have ~30 vhosts and a growing peer mesh. Manual edits don't scale.
- meerkat is the config-as-code repo for *this* deployment of orca.
  Orca itself is a generic framework; the plugin must work for any
  consumer who points orca at their own config repo.

---

## 2. Architecture

```
┌────────────────────────── meerkat repo (git, source of truth) ──────────┐
│  compose/caddy/Caddyfile.base         ← static / hand-written           │
│  compose/caddy/routes/*.toml          ← declarative routes              │
│  compose/caddy/routes/peers.toml      ← orca-peer fanout                │
└──────────────────────────────────────────────────────────────────────────┘
                                  │ git pull
                                  ▼
┌──────────────────── orca on baldur (caddy plugin) ───────────────────────┐
│  config store        ← imported routes + runtime overlay                 │
│  reconciler          ← renders Caddyfile.generated, validates, reloads   │
│  cert manager        ← mints + rotates Caddy's peer cert                 │
│  MCP/REST endpoints  ← caddy_route_{list,create,update,delete,...}       │
└──────────────────────────────────────────────────────────────────────────┘
                                  │ admin API on unix socket
                                  ▼
                              caddy container
```

Generic orca: ships the **caddy plugin** (reconciler, cert flow,
endpoints). Knows nothing about meerkat or scottkey.me.

Meerkat-side: ships the **declarative route files** + the Caddy compose
stack. Orca reads `compose/caddy/routes/` from the repo path it's
pointed at via bootstrap.

---

## 3. Config model

### 3.1 Declarative routes (meerkat, git-tracked)

`compose/caddy/routes/orca.toml`:

```toml
[[route]]
hostname = "orca.scottkey.me"
lb_policy = "round_robin"
health_uri = "/healthz"
health_interval = "10s"

  [[route.upstream]]
  peer = "frigg"          # resolved via orca peer table
  port = 12002
  mtls_peer = true        # plugin injects Caddy's client cert

  [[route.upstream]]
  peer = "thor"
  port = 12002
  mtls_peer = true

  [[route.upstream]]
  peer = "baldur"
  port = 12002
  mtls_peer = true

  [[route.upstream]]
  peer = "freyr"
  port = 12002
  mtls_peer = true
```

`compose/caddy/routes/infra.toml` (typical single-upstream entry):

```toml
[[route]]
hostname = "opn.scottkey.me"
[[route.upstream]]
addr = "10.10.10.1"
port = 80
```

Resolution rules:
- `peer = "<hostname>"` → looked up in orca's peer table at reconcile
  time. If unreachable, the upstream is still rendered (Caddy's
  passive health handles transient drops); if the peer is unknown,
  reconcile fails with a clear error.
- `addr = "..."` is the escape hatch for non-orca targets (Proxmox,
  Unifi, LXCs, etc.).
- `mtls_peer = true` triggers `transport http { tls_client_certificate_file ... }`
  with the cert the plugin mints for the local Caddy.

### 3.2 Runtime overlay (orca config store)

Mutations via MCP/REST land in the config store as an **overlay** on
top of the repo-tracked state. Two write modes:

- `persist = "runtime"` (default): in-store only. Survives reload.
  Lost on store wipe.
- `persist = "repo"`: plugin writes the TOML file back into the repo
  and (optionally) commits via the existing git plugin. This is how
  ad-hoc additions graduate to source-of-truth.

This is the load-bearing decision. It keeps git as the truth without
making every API call require a commit, and gives a clean promotion
path.

### 3.3 Generated artifact

Plugin renders `compose/caddy/Caddyfile.generated`. Compose imports it
via:

```
import /etc/caddy/Caddyfile.generated
```

`Caddyfile.base` keeps the global block (ACME, snippets) and anything
the plugin doesn't manage yet. Lets us migrate incrementally.

---

## 4. mTLS / cert management

- Plugin mints a peer-style cert for the Caddy container using orca's
  existing pod-mesh CA.
- Cert + key written to a docker volume the Caddy container mounts
  read-only.
- Plugin rotates on a schedule (reuse `system/schedule`); on rotation,
  reload via Caddy admin API — no container restart.
- The orca peer-cert validator on each target peer needs to accept
  Caddy's cert. Either:
  (a) Caddy is a paired peer in the pod (cleanest, reuses everything), or
  (b) plugin maintains a separate "client" cert type with its own
      trust anchor entry.
- **Recommendation:** (a). Caddy gets a `peer_id` and shows up in
  `peer_list`. Mesh ACLs stay uniform.

---

## 5. MCP / REST endpoints

Added to the plugin's surface (mirrors existing orca tool naming):

```
caddy_route_list
caddy_route_detail  --hostname X
caddy_route_create  --hostname X --upstream ... [--persist repo|runtime]
caddy_route_update  --hostname X ...
caddy_route_delete  --hostname X [--persist ...]
caddy_reload                       # idempotent
caddy_validate                     # render + caddy validate, no reload
caddy_status                       # admin API health
caddy_cert_status
caddy_cert_rotate
```

Federation: any paired peer can call these on baldur via the existing
MCP federation surface. No new auth code needed.

---

## 6. Reconciler

Loop:

1. Read routes from repo path + overlay from config store.
2. Resolve `peer = "..."` references against peer table.
3. Render `Caddyfile.generated` to a temp file.
4. `caddy validate --config <tmp>`.
5. On success: atomic rename → POST `/load` to admin API on unix
   socket → emit ntfy on failure.
6. On failure: leave previous generated file in place, return error
   to caller, alert.

Triggers: explicit `caddy_reload`, repo file change (fsnotify), config
store mutation, peer add/remove events.

Container ops (cert volume mount, the rare hard restart) go through
orca's rust-native docker engine bindings — not a shell-out to
`docker compose`. Reload via Caddy admin API stays the hot path; the
container lifecycle path is only for cert-volume changes or upgrades.

---

## 7. Migration from current Caddyfile

One-time importer: parse current `Caddyfile` → emit
`routes/*.toml` grouped by section comment (`Infrastructure`,
`Media Stack`, etc., already present as `# ──` headers). Commit
result. Switch the compose stack to `Caddyfile.base` + `import
Caddyfile.generated`. Delete the old Caddyfile.

Risk: the importer must round-trip the `tls_backend` snippet usage
and `header_down Location` rewrites. ~30 vhosts; manual spot-check is
cheap.

---

## 8. IPv6 plan

Three layers, each independent:

### 8.1 Caddy listener
Caddy binds `::` by default; nothing to do unless we want explicit
bind addresses. Verify with `ss -ltnp` inside the container post-deploy.

### 8.2 Peer fanout
Route TOML grows an optional v6 upstream slot, but the typical case
is: if `peer = "X"` and X has a routable v6 in `peer_list`, the
reconciler emits both v4 and v6 upstreams. No config change per
route. Today, only `mint` has a routable v6 (`2607:...`); the others
are Tailscale v6 (`fd7a:...`) which is fine for Tailscale-internal
fanout but not for public.

### 8.3 Cloudflare AAAA
Out of scope for v1 of the plugin. When ready: add a cloudflare
integration (or extend the existing one if any) that publishes AAAA
for hostnames whose upstream set includes a public-v6 peer. Until
then, public v6 stays disabled.

Practical implication for the "any orca can access any other" goal:
inside Tailscale, v6 already works peer-to-peer. The public-v6 story
is a Cloudflare DNS change, not a Caddy change.

---

## 9. "Any orca can expose/view any other" — what's needed

Beyond this plugin:
- mesh ACL: which peer-ids may call `caddy_route_create` on baldur.
  Today federation is all-or-nothing per paired peer; we likely want
  per-tool ACLs before opening this up. Track separately.
- DNS automation: creating a route is half the story; the hostname
  has to resolve. Either (a) restrict managed hostnames to `*.scottkey.me`
  with a wildcard, or (b) plugin talks to Cloudflare to create A/AAAA
  on demand. (a) is simpler and probably right for v1.

---

## 10. Open questions

- **Caddyfile vs JSON config?** Caddy's JSON config + admin API is
  more programmatic and avoids a render step. Tradeoff: harder to
  diff in git, less familiar. Leaning Caddyfile-text for human
  reviewability of the generated artifact.
- **Where does `Caddyfile.base` live in the runtime?** Probably
  mounted from the meerkat repo path directly, same as today.
- **Single Caddy or HA pair?** Out of scope here; orca-peer fanout
  gives upstream HA, but Caddy itself on baldur is still a SPOF.
  Track in the OPNsense-HA or a sibling doc.
- **Promotion UX**: when someone creates a route at runtime with
  `persist = "runtime"`, do we surface a "promote to repo" command,
  or auto-promote after N hours? Defer.

---

## 11. Work breakdown

| # | Item | Size |
|---|------|------|
| 1 | Plugin skeleton in orca: crate, plugin registration, empty MCP tools | S |
| 2 | Route TOML schema + loader + repo-path resolution from bootstrap | S |
| 3 | Renderer: routes → Caddyfile text; golden tests against current vhosts | M |
| 4 | Reconciler: render → validate → admin-API reload; failure alerting | M |
| 5 | Cert flow: mint peer cert for Caddy, mount, rotate via scheduler | M |
| 6 | MCP endpoints: list/detail/create/update/delete/reload/validate | S |
| 7 | Runtime overlay + `persist = repo` write-back (uses git plugin) | M |
| 8 | Caddyfile importer (one-shot migration tool) | S |
| 9 | Cut meerkat over: introduce `Caddyfile.base` + `routes/*.toml`, retire old Caddyfile | S |
| 10 | `orca.scottkey.me` with round-robin across reachable peers (the original ask) | S |
| 11 | Per-tool mesh ACL for `caddy_route_*` (separate doc, blocks "any peer" goal) | M |
| 12 | IPv6: reconciler emits v6 upstreams when peer has routable v6 | S |
| 13 | Cloudflare AAAA automation | M |

Items 1–10 unblock the original request. 11–13 are the path to the
"any orca can expose any other" vision.
