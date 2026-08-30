# Unified Media Acquisition — one pipeline, many providers

> Design doc. Extends the capability-registry platform
> (`docs/CAPABILITY-REGISTRIES.md`) into the media domain. Status: **design /
> long-term**. No near-term build commitment.
>
> **Hard dependency / build order.** This design binds to principals, groups,
> and per-user credentials that do not exist yet. The identity / SSO / RBAC
> layer is a **prerequisite, not a peer** — purchase authorization, the
> download approval flow, and credential brokerage all resolve against it.
> Build sequence: **identity → RBAC/groups → acquisition**. Do not start the
> purchase/approval seams before the identity layer is frozen.

## The one idea

**Every media type follows the same pattern.** TV, music, movies, comics,
books, audiobooks — all of them go through one uniform lifecycle:

```
search → (download | purchase) → file into the right folder
       → trigger the right server to update → manage + correct metadata
```

No media type is a special case. orca provides **one set of verbs** and the
media type is a parameter, exactly as `service.*` treats a VM, LXC, or container
uniformly. These orca capabilities are built **on top of** the underlying
services (the *arr apps, Mylar, Komga, Navidrome, Audiobookshelf, Calibre,
Plex/Jellyfin) to present **one unified interface** — orca is the unifying
layer; the services are backends.

This is the `CAPABILITY-REGISTRIES.md` pattern applied to media: core holds the
abstractions (an `AcquisitionSource` trait + a media-management/metadata trait +
registries + the pipeline engine); every concrete source or backend — SABnzbd,
qBittorrent, Bandcamp, Libro.fm, DriveThruComics, MakeMKV, and the metadata
providers — is an external plugin that registers in.

### Structure: one vocabulary, one dispatch table, two tails

There is **one shared vocabulary** (`search / acquire / file / scan / identify /
organize`) and **one dispatch table** (the media-type → backend routing below).
There is **not** one code path: each media type is a bespoke adapter behind that
shared vocabulary, and there are **two acquisition tails**, governed differently:

- **Downloader tail (the *arr apps, unchanged).** Sonarr/Radarr/Lidarr/Mylar
  keep their **full autonomous loop** — discovery, grab, import, rename — and it
  stays **ungated**. orca does **not** intercept their grabs (they grab
  autonomously via RSS/auto-search; there is no pre-grab veto hook, so trying to
  gate the grab would mean disabling the autonomy users want). Files are filed
  and scanned by the *arr's own import step.
- **Native tail (new, for purchases + owned-disc rips).** No *arr owns this
  path, so orca **is** the pipeline: `acquire → staging → organize → file →
  scan`.

Unified search and policy sit **above** both; the native tail sits **beside**
the downloader stack, not replacing it.

### The download gate: library membership, not the grab

The *arr grab cannot be intercepted, so the control point is **adding a title to
the system** — the Overseerr/Jellyseerr model:

```
user submits an ADD request  ("add this movie/show/album")
   → media-admins APPROVE the addition
   → orca creates the monitored item in the right *arr (its add-API)
   → the *arr auto-download AS NORMAL, ungated
```

The decision *"should this be in the library"* is the admin's; the mechanics of
*how to grab it* stay the *arr's. The add-request, the approver, and the
resulting monitored item are attributed and audited. This gate operates without
altering the downloader apps.

## The uniform lifecycle across media types

One verb surface; the media type selects the requester, the library path, and
the server to notify. Nothing below is type-specific logic in core — it is a
table of backends behind the same seams.

| Media | Downloader owner | Library path | Server to refresh | Metadata source |
|---|---|---|---|---|
| TV | sonarr | `/data/media/tv` | Plex/Jellyfin | TVDb / TMDb |
| Movies | radarr, radarr-4k | `/data/media/movies`, `/4k` | Plex/Jellyfin | TMDb |
| Music | lidarr | `/data/media/music` | Navidrome | MusicBrainz (beets) |
| Comics | mylar3 | `/data/media/comics` | Komga | ComicVine |
| Books | readarr / LazyLibrarian | `/data/media/books` | Calibre / Calibre-Web | Google Books / ISBN / OpenLibrary |
| Audiobooks | LazyLibrarian | `/data/media/audiobooks` | Audiobookshelf | Audible / OpenLibrary |

## Unified search + acquire-vs-buy

Acquisition begins with a **search** that fans out across *every* source — the
downloader indexers **and** purchased-source catalogs — and presents a merged
result set keyed to a media identity. The merge is **presentational plus a
policy default**, not a single ranking: download hits (quality, size, seeders)
and purchase hits (price, format, DRM-free) are not one comparable axis. The
policy default is:

> **Prefer an already-owned / purchased copy** over a fresh download when one
> exists — noting this dedupe only works for canonically-identified media
> (a TMDb movie owned-on-disc vs a download); for the indie long tail the owned
> copy and the download often don't share an id, so the preference silently
> no-ops there.

For each hit the user (or a standing policy) chooses **per result**: **add-for-
download** (create a monitored item in the *arr, subject to the add approval
above) or **purchase** (buy the DRM-free copy). The two do **not** share one
tail: a download is filed and scanned by the *arr's own import; only a purchase
runs orca's native `acquire → staging → organize → file → scan`. One vocabulary,
two tails, governed differently (below).

## Acquisition authorization

Downloading and purchasing carry different risks and are gated differently.

### Adding a title → request, then media-admin approval

The gate is on **library membership** (adding a title), not on the grab — the
*arr grab autonomously and cannot be vetoed mid-flight:

- **Most users cannot add titles directly.** An ordinary / `media`-group user
  may only **request an addition**.
- **Only the `media-admins` group may approve** an add request. On approval,
  orca creates the monitored item in the appropriate *arr and the *arr
  auto-download as normal.
- The add-request → approver → resulting monitored item is **attributed to the
  requesting user** in a hash-chained audit log.

```
add-request(title, requesting_user)
  → queued as a pending request
  → a media-admins member approves (or denies)
      approved → orca creates the monitored item in the *arr → *arr auto-grabs
      denied   → closed, nothing added
```

A standing **auto-add / auto-download policy is confined to titles already in
the library and to the purchase/native side** — it must never create a *new*
library addition without an approval record, or it silently bypasses this gate.

### Purchases → per-user, self-service when enabled

Purchasing spends money on legitimate DRM-free content and is **self-service**,
conditional on the capability being enabled for that user:

- **Purchasing is a per-user capability grant.** Off → the user cannot purchase
  at all. On → the user buys on their **own authority, no per-purchase
  approval**.
- Bounded by **spend controls** (below), never unbounded.

Adding a title to the shared system is an admin decision; purchasing DRM-free
content is a per-user decision gated only by the capability grant and spend
controls.

## Per-user purchase accounts + mesh sharing

- **Each user brings their own account.** A user (e.g. `skey`) signs into a
  legitimate purchase source; that credential is bound to *that user's*
  identity and stored through orca secrets as an opaque reference — never
  plaintext at rest.
- **Broker-only sharing.** `skey` may share the *ability to purchase* through
  `skey`'s source with specific other mesh users, opt-in and revocable,
  user-by-user. The grantee **never receives the raw credential** — the node
  holding the secret executes the purchase on the grantee's behalf (a session
  cookie or OAuth token is all-powerful at the store, so it must never leave the
  owner's node).
- **Anyone can add their own source** and choose whether to share it.

**Spend controls (mandatory). The broker node holds a powerful credential and
acts on other users' requests, so each control is required:**

- **Single-writer budget on the owner's node.** Broker-only routing sends
  *every* purchase against an account through that account's owner node, so the
  budget counter is a **local single-writer** value with **reserve-then-commit**
  (hold the amount before charging, settle or release after). The counter never
  lives on the gossip bus, which prevents distributed double-spend that an
  eventually-consistent counter cannot. Refunds **credit the hold back** through
  the same single writer, so buy→refund→buy neither inflates nor permanently
  burns the cap.
- **Per-grantee aggregate ceiling**, not just per-grant — a grantee with grants
  from several owners still has one total ceiling, so many small grants can't
  sum to an uncontrolled total.
- **Idempotency key + rate limit per request** — a replayed or scripted flood of
  sub-cap requests is deduped and throttled, not charged N times.
- **Price pinned and re-quoted on the owner's node** before charging, and the
  confirm-threshold is evaluated against the **store-authoritative** price, not
  a requester-asserted one — so a stale/cheap quote can't slip an expensive item
  under the threshold, and the wrong SKU/bundle can't be charged.
- **Out-of-band confirmation to the card owner** (not the requesting node) for
  above-threshold and all shared-account charges; a confirmation the initiator
  can forge is not a control.
- **Hash-chained / signed audit log** attributing each charge to the requesting
  user. The owner's node is sole writer and holds the card, so the chain makes
  tampering evident; entries are written at execution time on the owner node so
  partition-time charges are not lost.
- **Kill-switch via freshness-SLA fail-closed** (see identity, below): the owner
  node refuses to purchase if it hasn't confirmed current kill-switch / grant /
  authz state within N seconds — so a partitioned node stops buying within N
  seconds rather than running unbounded.

**Consent model.** An enabled grant is the owner's standing consent up to the
budget; the owner does **not** see every grantee charge, only above-threshold
ones (out-of-band). orca's attribution is for internal accountability, **not** a
guarantee of card-network chargeback standing — the store only ever sees the
owner.

**Failure states.** A purchase provider's `status` surfaces `needs-human` (2FA /
CAPTCHA / device verification), with a **defined notification transport, a
timeout, and a terminal state** if unanswered. Also `charged-but-undelivered`,
`refunded`, and `account-locked`. API tokens / app-passwords are preferred over
scraped session cookies where a source offers them; a scraped cookie on the
broker node is account-takeover-equivalent.

**Ownership scope of the acquired file.** A purchased / ripped file defaults to
the **acquiring user's own scope**, *not* the shared `media`-group library.
Publishing it into a shared library is a separate, explicitly-authorized step.
This keeps the personal-format-shift footing intact and avoids "one purchase
silently becomes fleet-wide redistribution."

```
purchase(item, requesting_user)
  → is purchasing enabled for requesting_user?           no → deny
  → which registered accounts can fulfill this item?
  → is requesting_user authorized on one (own, or shared to them)?  no → deny
  → within budget / under confirm-threshold?             no → require confirm
  → owner's node executes the purchase (broker-only), audit the charge
  → file to requesting_user's scope → pipeline
```

## Identity, SSO, and credential brokerage (prerequisite subsystem)

A purchase account is one instance of orca's per-user **credential brokerage**
(see the identity/SSO design). orca stores, rotates, and *presents* each user's
own managed credentials for the services they're enabled on, so they configure
their own devices.

This is a **greenfield auth subsystem** (groups table, permission-sets,
mesh-signed JWTs, webauthn, forward_auth, OIDC, AccountBackend) that **replaces**
the current server-side-session + single-role-string auth. Build sequence:
**identity → RBAC → acquisition.**

- **Access is gated by top-level groups.** A **media** group provisions
  consumption (Navidrome, Jellyfin, Komga, Audiobookshelf, Calibre readers); a
  **media-admins** group provisions the administration stack (Sonarr, Radarr,
  Lidarr, LazyLibrarian, Mylar, Prowlarr, SAB/qBit) **and** holds add-request
  approval authority. Groups are referenced by **uuid, not name**. They ship as
  a seed but are ordinary runtime objects; to avoid the mesh resurrecting a
  deleted group, **only the first node of a fleet seeds — joining nodes inherit
  groups via replication and never run local seed** (a fresh node's re-seed
  would write a newer op that supersedes the delete tombstone). The **undeletable
  root admin grant** needs a **replication-layer protected-key guard** (today's
  delete path is generic, with no invariant), plus a loopback break-glass
  re-seed if it is ever lost.
- **Consistency for critical state: freshness-SLA + fail-closed.** The mesh is
  wall-clock last-write-wins with no quorum and no central store, so a "live
  authz check against the identity store" would read the *same stale replica* as
  the token it distrusts. Instead: keep the gossip, but **a node refuses a
  high-risk action unless its replica synced within N seconds** (fail closed on
  staleness). This bounds revocation / grant / kill-switch staleness to N
  seconds without adding a consensus layer. High-risk = purchase, credential
  retrieval, admin writes, add-request approval; low-risk (stream/browse) trust
  the local replica. The revocation epoch must be **monotonic (reject any lower
  value)** so a concurrent whole-row LWW write under clock skew can't silently
  revert it.
- **Stateless tokens; forward_auth constraints.** Tokens are mesh-signed
  JWTs validated by signature; signing-key rotation uses overlapping windows
  sized to exceed worst-case gossip lag. Caddy `forward_auth` must be
  **fail-closed** *and* load-balance across **multiple** orca nodes (a single
  target is a per-request SPOF that negates statelessness). Short TTLs need a
  **silent refresh** path so a 15-minute token doesn't interrupt an in-flight
  Jellyfin stream.
- **Credential retrieval limits.** Retrieval is unconditionally high-risk and
  scoped to the caller's **own principal** (a user can never fetch another user's
  credential). Removing a user from a group cannot claw a credential back off
  their device; it triggers **rotate + service-side session-revoke where the
  backend supports it**. Where a backend only has a static password (Navidrome,
  Calibre-Web, *arr), revocation = rotate (old cred dies at next reconcile, a
  residual-access window).
  Prefer **per-device app-passwords** so rotating one device doesn't log out the
  others. AccountBackend projection has a **single owner-node per (service,
  account)** (same single-writer discipline as metadata paths) so two nodes
  don't race to set the same app-password.
- **Passkeys need a fixed RP-ID strategy** for the multi-origin homelab (LAN IP
  vs tailscale name vs public domain), or a passkey registered on one origin
  fails to authenticate on another.

## The capability surface (verbs a provider implements)

The universal contract is **`acquire → DRM-free file` + `status`**: those two are
meaningful for every provider and form the trait. Everything else
(`authenticate`, `search-catalog`, `list-owned`) is a **capability-flagged
optional**: `authenticate` covers several unrelated mechanisms, `search-catalog`
and `list-owned` are n/a for downloaders, and scrape-based sources are flagged
**best-effort / fragile** (they break when a store changes its account-page HTML,
and their `status` must catch a login-page-returned-as-HTTP-200). The surface is
two universal verbs plus optional extras, not one uniform seam set.

| Verb | Downloader provider | Purchase / rip provider |
|---|---|---|
| `authenticate` | client host + api key (SAB/qBit) | per-user: API key (DriveThru, itch), OAuth (Gumroad), session cookie (Humble, Bandcamp, Libro.fm), app registration |
| `search-catalog` | indexer search (torznab/newznab) | store catalog search (where exposed) |
| `list-owned` | n/a | enumerate the account's owned library |
| `acquire(item)` → DRM-free file | grab NZB/torrent → completed file | download the owned/purchased item, emit EPUB/M4B/CBZ/FLAC/MKV |
| `status` | queue/download progress | download/purchase progress + `needs-human` / `refunded` / `charged-but-undelivered` / `account-locked` |

Files land in the library paths and trigger scan-on-import. Scan-on-import is
**not** uniform under the hood — Komga, Navidrome, Audiobookshelf, Calibre-Web,
Plex, and Jellyfin each have their own rescan trigger, and at least one
(Navidrome) may have no reliable on-demand scan API at all (schedule/watch only).
The dispatcher is a set of **per-backend adapters** behind one verb. Retry is
**not** the fix for the Lidarr rescan-wedge: that wedge is head-of-line blocking
*inside Lidarr's own serialized command queue* (a `RescanFolders` stuck on an SMB
sharing-violation while beets moves files), it persists in `lidarr.db`, and it
**cannot be cancelled via the API** (`DELETE` returns 409 on a started command).
The remedy is **contention avoidance**: do not run a full library rescan while a
bulk move is in flight (serialize the two).

## Media management + metadata

Once a file is in the library, orca manages it and maintains correct metadata.
Uniform verbs, with a **single-writer rule enforced via staging**:

> **Each library path has exactly one authoritative organize owner.** An *arr's
> import step writes the library path (it moves and renames the completed file),
> and beets also writes; both cannot be read-only. The rule is realized as a
> **pipeline shape**: acquire → **staging (scratch) path** → the one owner
> organizes → library. The *arr import lands in its own owned area; a second tool
> (beets) operates on a different owned path or in a serialized handoff — never
> two writers on one live path at once. A two-live-writers config is flagged; the
> remedy is the staging handoff.

| Verb | What it does | Routed to the path's owner |
|---|---|---|
| `identify(file/item)` | resolve to a canonical id; unresolved → dead-letter (below) | ComicVine, MusicBrainz, TMDb/TVDb, ISBN/OpenLibrary |
| `refresh-metadata(item)` | (re)fetch correct tags/art/chapters and write them | beets, Mylar/ComicVine, *arr refresh, Calibre, Audiobookshelf |
| `organize(item)` | rename/relocate to the naming convention | the one owner for that path |
| `dedupe / reconcile` | detect duplicates; prefer owned/purchased over downloaded | pipeline policy |
| `verify` | confirm files are readable + match expected metadata | per-server scan + checksum |

`refresh-metadata` is **provider-aware for *identity***, which is distinct from
skipping work. A purchased file's identity is trusted (not re-*identified*), but a
trusted identity does not supply the library-native naming/tagging: a Bandcamp
FLAC or Humble PDF still needs the full organize/tag pass to become
Komga/Plex/Audiobookshelf-readable. The shortcut only avoids *re-processing* for
sources whose native format already matches the library convention
(DriveThruComics CBZ, Libro.fm M4B), not for the fragile scrape sources. When
identity resolution fails (indie Bandcamp release, unlabeled disc rip, bundle-only
edition with no canonical id), the item goes to a **`_unmatched/<source>/`
dead-letter** for manual match, never guess-filed. For indie sources, canonical-id
coverage (MusicBrainz/ComicVine/ISBN) is low, so `identify` and the "prefer owned
copy" dedupe may no-op across much of the long tail; the metadata capability is
scoped as "manual-assist for indie" pending a match-rate sample.

## What existing plugins do (and don't) change

- **Downloader apps (sonarr/radarr/lidarr/mylar): unchanged internally, full
  autonomy retained.** They keep their own download-client wiring, discovery,
  grab, and import. orca does **not** gate their grabs — it gates **library
  membership**: an add-request is approved by a media-admin, then orca calls the
  *arr add-API to create the monitored item, after which the *arr auto-download
  as normal. Discovery of *what to add* moves to orca's request queue; *how to
  grab it* stays the *arr's.
- **New purchase/rip providers** implement the capability surface above and run
  the native orca pipeline (the second tail).
- **Unified search, add-request approval, purchase spend control, and
  metadata-routing** are the new orca layer above both — a dispatcher and a
  front door, not a replacement for the *arr import pipelines.

---

## Appendix — DRM-free source catalogue (surveyed 2026-08-30)

**Native-DRM-free sources only.** orca ships and orchestrates **no** de-DRM /
circumvention tooling (no kobodl, Calibre-DeDRM, ACSM→Adobe, Libation-decrypt).
Distributing de-DRM tooling as a multi-user fleet feature is a §1201
provision-of-tooling exposure distinct from one person format-shifting their own
book — so it is out of scope entirely. Supported sources are those that are
**DRM-free by design** (or public-domain, or owned-physical-disc rips). Every
mainstream DRM store (iTunes, Amazon, Netflix, Disney+, ComiXology/Kindle,
Movies Anywhere, Kanopy/Hoopla) is excluded — acquisition there would require
circumvention.

### Music
- **Bandcamp** — primary music source; DRM-free purchased downloads; tools: `bandcampsync`, `bandcamp-dl` (cookie/collection auth — flagged fragile).
- **Qobuz** — FLAC purchases, DRM-free; second priority.
- Apple / Amazon Music / HDtracks / 7digital — DRM-free *files* but no automatable owned-library API → manual only.

### Books / eBooks
- ✅ **Standard Ebooks** — public-domain, official **OPDS** feed, no auth.
- ✅ **Humble Bundle** — DRM-free EPUB/PDF/CBZ; `/api/v1/order` JSON + session cookie; tool: `xtream1101/humblebundle-downloader`.
- ✅ **Tor / No Starch / Smashwords / à-la-carte O'Reilly** — DRM-free, session-cookie scrape (fragile).
- ⛔ **Kobo, Google Play Books (ACSM), Amazon Kindle, O'Reilly Learning** — DRM / de-DRM-required → **excluded** by the native-only decision.

### Audiobooks
- ✅ **Libro.fm** — DRM-free M4B/MP3; tools: `burntcookie90/librofm-downloader` (Audiobookshelf-friendly), `libro-client`.
- ✅ **Downpour** — DRM-free; `em-downpour-downloader`.
- ⛔ **Audible (Libation), Kobo audiobooks** — require decryption → **excluded** by the native-only decision. (Libation may still be run by a user independently; orca just won't ship/orchestrate it.)

### Comics
- ✅ **DriveThruComics** — official app-key API, native CBZ, incremental sync; tool: `drpg`.
- ✅ **itch.io** — official API key + `owned-keys` endpoint; implement natively.
- ✅ **Humble Bundle (comics)** — same downloader as books; CBZ/CBR.
- ⚠️ **Gumroad** — OAuth v2 works but buyer-library is weak (seller-centric); license-key / download-link ingestion.
- Partial (session-scrape, no API, fragile): **GlobalComix** (DRM-free PDF), **2000 AD** (native CBZ).
- Watch: **Image via Sweet Shop** (DRM-free PDF, no API yet). Dead/excluded: Dark Horse Digital (shut Mar 2025), ComiXology/Kindle (DRM).

### TV / Movies (hardest — essentially owned-disc ripping)
- ⚠️ **MakeMKV** (`makemkvcon`) — owned-disc rip → unencrypted MKV. **Attended / human-in-the-loop, NOT an unattended fleet provider.** Three hard constraints: (1) the free build runs on a **beta key that expires ~every 60 days** with no key API (checked at program start) — expiry silently stops rips; (2) requires a **physical optical drive per host** (and a flashed **LibreDrive**-compatible drive for 4K UHD), so only drive-equipped nodes can serve it — it breaks the "any node fulfills" model; (3) **title identification** (main feature vs extras vs per-episode TV) is a heuristic that misfiles unattended. Model it like **Automatic Ripping Machine** (udev disc-insert → MakeMKV → HandBrake → file) where `needs-human` is the *normal* state, not the exception. Transcode: **HandBrakeCLI**. Frame strictly as personal format-shifting of physically-owned discs.
- ✅ **Internet Archive** — public-domain video; official `internetarchive` lib / `ia` CLI.
- Bespoke: **Gumroad / itch.io / Payhip** where a creator sells raw MP4/MKV — generic "download owned files."
- ⛔ All mainstream digital stores + streaming + library-streaming (Widevine/FairPlay/PlayReady). **Vimeo On Demand shuts down Nov 2026** — do not build around it.

### Stability note
Most purchase sources change frequently (dead services, removed download paths,
fragile scrapes). Stable, low-risk targets: **Standard Ebooks (OPDS), DriveThruComics (API), itch.io (API),
Internet Archive (CLI), MakeMKV (disc)**. Prioritize those; treat the rest as
best-effort.

### Cross-cutting
- Register each source/account **once** in the define-once registry; reference everywhere.
- Identity mapping: purchased item → library metadata id (MBID / ISBN / ComicVine / TMDb / TVDB). Unresolved → `_unmatched/` dead-letter. Normalize toward the library-native format (prefer CBZ over PDF for Komga/Mylar; chapterized M4B for Audiobookshelf).
- Keep all repo language piracy-free; ship no circumvention *code* (the native-only decision is the control).
