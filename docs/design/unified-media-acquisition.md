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

### Two acquisition classes — orca does NOT rewrite the *arr apps

The existing *arr apps own a fat internal loop (indexer search → release
decision → download-client polling → import/rename). They are **not** demoted to
thin requesters — that fights their design and creates dual-owner-of-file
churn. Instead there are two provider classes, and unified search / policy sit
**above** both:

- **Downloader stack (left as-is).** Sonarr/Radarr/Lidarr/Mylar keep their own
  download-client wiring and import pipeline. orca orchestrates them via their
  APIs; it does not restructure their internals.
- **Native purchase/rip providers (new).** For purchased DRM-free sources and
  owned-disc rips, orca **is** the pipeline (search → acquire → file → scan),
  because no *arr owns that path.

The **unified pipeline handles the purchase/native path and sits beside the
downloader stack** — it does not replace it.

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
policy default**, not a single magic ranking: download hits (quality, size,
seeders) and purchase hits (price, format, DRM-free) are not one comparable
axis. The load-bearing rule is the policy default, and it is:

> **Prefer an already-owned / purchased copy** over a fresh download when one
> exists.

For each hit the user (or a standing policy) chooses **per result**: **acquire
(download)** or **purchase (buy the DRM-free copy)**. Both feed the same
`acquire → file → scan` tail — but they are governed very differently (below).

## Acquisition authorization — the asymmetry

Downloading and purchasing carry different risks, so they are gated
differently. **The riskier action is gated harder.**

### Downloads → request, then media-admin approval

Torrent/usenet downloading is shared legal exposure to the whole system, so it
is the **most-gated** action:

- **Most users cannot download directly.** An ordinary / `media`-group user may
  only **request** a download.
- **Only the `media-admins` group may approve** a download request. No approval
  → no grab.
- Every approved grab is **attributed to the requesting user** in an
  append-only audit log.

```
download-request(item, requesting_user)
  → queued as a pending request
  → a media-admins member approves (or denies)
      approved → dispatch the grab, attribute it to requesting_user, audit
      denied   → closed, no grab
```

### Purchases → the user's own decision, when enabled

Purchasing spends (the user's own or granted) money on legitimate DRM-free
content, so it is **self-service** — but only if the capability is switched on
for that user:

- **Purchasing is a per-user capability grant.** Off → the user cannot purchase
  at all. On → the user buys on their **own authority, no per-purchase
  approval**.
- Bounded by **spend controls** (below), never unbounded.

The elegance is deliberate: a download needs an admin in the loop because it
exposes the system; a purchase of legit content is the user's call once you've
trusted them with the capability.

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

**Spend controls (mandatory on every purchase grant):**

- Per-grant **budget** — period cap + per-transaction cap.
- **Confirmation threshold** — auto-buy under $X; human-confirm above.
- Append-only **purchase audit log** attributing each charge to the requesting
  user (the store only sees the owner's account — orca must keep the true
  attribution for disputes/chargebacks).
- Global **kill-switch**.

**Failure states are first-class, not exceptions.** A purchase provider's
`status` surfaces `needs-human` (2FA / CAPTCHA / device verification — never
silently stall), `charged-but-undelivered`, `refunded`, and `account-locked`.
Prefer API tokens / app-passwords over scraped session cookies wherever a source
offers them.

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

## Identity, SSO, and credential brokerage (dependency)

A purchase account is one instance of orca's broader **per-user credential
brokerage** (see the identity/SSO design). orca stores, rotates, and *presents*
each user's own managed credentials for the services they're enabled on, so they
configure their own devices.

- **Access is gated by top-level groups.** A **media** group provisions
  consumption (Navidrome, Jellyfin, Komga, Audiobookshelf, Calibre readers); a
  **media-admins** group provisions the administration stack (Sonarr, Radarr,
  Lidarr, LazyLibrarian, Mylar, Prowlarr, SAB/qBit) **and** holds
  download-approval authority. SSO grants login to each app only where the
  user's groups allow it. Groups ship as a seed but are ordinary runtime
  objects — referenced by **uuid, not name** (renames are safe), guarded
  against deleting a group other users/verbs depend on, with one **undeletable
  root admin grant** as the hard floor.
- **Revocation that actually works.** Tokens are stateless mesh-signed JWTs
  (any node validates by signature — mesh data is eventually consistent, so no
  server-side sessions), but with a **short TTL + a per-principal epoch** bumped
  on demote/disable, so any node rejects stale tokens ≈ mesh-propagation time
  rather than full expiry. **High-risk actions (purchase, credential retrieval,
  admin writes) do a live authz check** against the identity store rather than
  trusting the token's group claims; low-risk actions (stream, browse) trust the
  claims. Signing-key rotation uses overlapping key windows (no mid-flight
  cliff).
- **Credential retrieval is honest about its limits.** Removing a user from a
  group cannot claw a credential back off their device; it triggers **rotate +
  service-side session-revoke where the backend supports it**. Where a backend
  only has a static password (Navidrome, *arr), revocation = rotate (old cred
  dies at next reconcile). Prefer **per-device app-passwords** so rotating one
  device doesn't log out the others.

## The capability surface (verbs a provider implements)

Every provider implements the same seam set. Not every provider implements every
verb — the trait carries capability flags, and scrape-based sources are flagged
**best-effort / fragile** (they break when a store changes its account-page
HTML). This is a union behind a common shape, honestly labeled, not a promise
that every source is a clean API.

| Verb | Downloader provider | Purchase / rip provider |
|---|---|---|
| `authenticate` | client host + api key (SAB/qBit) | per-user: API key (DriveThru, itch), OAuth (Gumroad), session cookie (Humble, Bandcamp, Libro.fm), app registration |
| `search-catalog` | indexer search (torznab/newznab) | store catalog search (where exposed) |
| `list-owned` | n/a | enumerate the account's owned library |
| `acquire(item)` → DRM-free file | grab NZB/torrent → completed file | download the owned/purchased item, emit EPUB/M4B/CBZ/FLAC/MKV |
| `status` | queue/download progress | download/purchase progress + `needs-human` / `refunded` / `charged-but-undelivered` / `account-locked` |

Files land in the library paths and trigger scan-on-import. Scan-on-import is
**not** uniform under the hood — Komga, Navidrome, Audiobookshelf, Calibre-Web,
Plex, and Jellyfin each have their own rescan trigger, and some are fragile
(cf. the Lidarr rescan-wedge on an SMB lock). The dispatcher is a set of
**per-backend adapters** with lock-awareness and retry, behind one verb — not a
single call that magically fits all six.

## Media management + metadata

Once a file is in the library, orca manages it and guarantees correct metadata —
a first-class capability, not an afterthought. Uniform verbs, but with a **hard
single-writer rule**:

> **Each library path has exactly one authoritative metadata/organize owner.**
> `organize` / `refresh-metadata` *route to that owner*; a second writer on the
> same path is a configuration error orca refuses. (Two owners on one path is
> exactly the beets-vs-Lidarr contention that wedged rescans this session.)

| Verb | What it does | Routed to the path's owner |
|---|---|---|
| `identify(file/item)` | resolve to a canonical id; unresolved → dead-letter (below) | ComicVine, MusicBrainz, TMDb/TVDb, ISBN/OpenLibrary |
| `refresh-metadata(item)` | (re)fetch correct tags/art/chapters and write them | beets, Mylar/ComicVine, *arr refresh, Calibre, Audiobookshelf |
| `organize(item)` | rename/relocate to the naming convention | the one owner for that path |
| `dedupe / reconcile` | detect duplicates; prefer owned/purchased over downloaded | pipeline policy |
| `verify` | confirm files are readable + match expected metadata | per-server scan + checksum |

`refresh-metadata` is **provider-aware**: a purchased file carries a *trusted*
identity from its order and is not blindly re-identified; a scene download is
matched with confidence scoring. When identity resolution fails (indie Bandcamp
release, unlabeled disc rip, bundle-only edition with no canonical id), the item
goes to a **`_unmatched/<source>/` dead-letter** for manual match — it is never
guess-filed into the library.

## What existing plugins do (and don't) change

- **Downloader apps (sonarr/radarr/lidarr/mylar): unchanged internally.** They
  keep their own download-client wiring. orca orchestrates them and adds the
  request→approval gate in front of *triggering* a grab; it does not refactor
  their import pipelines.
- **New purchase/rip providers** implement the capability surface above and run
  the native orca pipeline.
- **Unified search, acquire-vs-buy, spend control, approval, and
  metadata-routing** are the new orca layer that sits above both.

---

## Appendix — DRM-free source catalogue (2026-08-30)

**Decision: native-DRM-free ONLY.** orca ships and orchestrates **no** de-DRM /
circumvention tooling (no kobodl, Calibre-DeDRM, ACSM→Adobe, Libation-decrypt).
Distributing de-DRM tooling as a multi-user fleet feature is a §1201
provision-of-tooling exposure distinct from one person format-shifting their own
book — so it is out of scope entirely. Supported sources are those that are
**DRM-free by design** (or public-domain, or owned-physical-disc rips). Every
mainstream DRM store (iTunes, Amazon, Netflix, Disney+, ComiXology/Kindle,
Movies Anywhere, Kanopy/Hoopla) is excluded — acquisition there would require
circumvention.

### Music
- **Bandcamp** — best target. DRM-free purchased downloads; tools: `bandcampsync`, `bandcamp-dl` (cookie/collection auth — flagged fragile).
- **Qobuz** — FLAC purchases, DRM-free; second priority.
- Apple / Amazon Music / HDtracks / 7digital — DRM-free *files* but no automatable owned-library API → manual only.

### Books / eBooks
- ✅ **Standard Ebooks** — public-domain, official **OPDS** feed, no auth. Cleanest, most stable source in the whole catalogue.
- ✅ **Humble Bundle** — DRM-free EPUB/PDF/CBZ; `/api/v1/order` JSON + session cookie; tool: `xtream1101/humblebundle-downloader`.
- ✅ **Tor / No Starch / Smashwords / à-la-carte O'Reilly** — DRM-free, session-cookie scrape (fragile).
- ⛔ **Kobo, Google Play Books (ACSM), Amazon Kindle, O'Reilly Learning** — DRM / de-DRM-required → **excluded** by the native-only decision.

### Audiobooks
- ✅ **Libro.fm** — DRM-free M4B/MP3; tools: `burntcookie90/librofm-downloader` (Audiobookshelf-friendly), `libro-client`. Best audiobook target.
- ✅ **Downpour** — DRM-free; `em-downpour-downloader`.
- ⛔ **Audible (Libation), Kobo audiobooks** — require decryption → **excluded** by the native-only decision. (Libation may still be run by a user independently; orca just won't ship/orchestrate it.)

### Comics
- ✅ **DriveThruComics** — best target: official app-key API, native CBZ, incremental sync; tool: `drpg`.
- ✅ **itch.io** — official API key + `owned-keys` endpoint; implement natively.
- ✅ **Humble Bundle (comics)** — same downloader as books; CBZ/CBR.
- ⚠️ **Gumroad** — OAuth v2 works but buyer-library is weak (seller-centric); license-key / download-link ingestion.
- Partial (session-scrape, no API, fragile): **GlobalComix** (DRM-free PDF), **2000 AD** (native CBZ — great content).
- Watch: **Image via Sweet Shop** (DRM-free PDF, no API yet). Dead/excluded: Dark Horse Digital (shut Mar 2025), ComiXology/Kindle (DRM).

### TV / Movies (hardest — essentially owned-disc ripping)
- ✅ **MakeMKV** (`makemkvcon`) — owned-disc rip → unencrypted MKV; flagship provider. First-class robot-mode CLI. Reference impl: **Automatic Ripping Machine** (udev disc-insert → MakeMKV → HandBrake → file). Transcode: **HandBrakeCLI**. 4K UHD needs a LibreDrive-compatible drive (surface in `detect-media`). Frame strictly as personal format-shifting of physically-owned discs.
- ✅ **Internet Archive** — public-domain video; official `internetarchive` lib / `ia` CLI. Clean and stable.
- Bespoke: **Gumroad / itch.io / Payhip** where a creator sells raw MP4/MKV — generic "download owned files."
- ⛔ All mainstream digital stores + streaming + library-streaming (Widevine/FairPlay/PlayReady). **Vimeo On Demand shuts down Nov 2026** — do not build around it.

### Stability note
Most purchase sources are hostile-to-automation moving targets (dead services,
removed download paths, fragile scrapes). The genuinely stable, low-risk targets
are few: **Standard Ebooks (OPDS), DriveThruComics (API), itch.io (API),
Internet Archive (CLI), MakeMKV (disc)**. Prioritize those; treat the rest as
best-effort.

### Cross-cutting
- Register each source/account **once** in the define-once registry; reference everywhere.
- Identity mapping: purchased item → library metadata id (MBID / ISBN / ComicVine / TMDb / TVDB). Unresolved → `_unmatched/` dead-letter. Normalize toward the library-native format (prefer CBZ over PDF for Komga/Mylar; chapterized M4B for Audiobookshelf).
- Keep all repo language piracy-free — and, more substantively, ship no circumvention *code* (the native-only decision is the real control; wording is hygiene, not the safeguard).
