# Unified Media Acquisition — one pipeline, many providers

> Design doc. Extends the capability-registry platform
> (`docs/CAPABILITY-REGISTRIES.md`) into the media domain. Status: **design /
> long-term**. No near-term build commitment; this is the target architecture
> that existing media plugins converge toward and that new purchased-source
> plugins are built against from day one.

## The one idea

**Every media type follows the same pattern.** TV, music, movies, comics,
books, audiobooks — all of them go through one uniform lifecycle:

```
search → purchase (where applicable) → download → file into the right folder
       → trigger the right server to update → manage + correct metadata
```

No media type is a special case. orca provides **one set of verbs** and the
media type is a parameter, exactly as `service.*` treats a VM, LXC, or container
uniformly. These orca capabilities are built **on top of** the underlying
services (the *arr apps, Mylar, Komga, Navidrome, Audiobookshelf, Calibre,
Plex/Jellyfin) to present **one unified interface** — orca is the unifying
layer; the services are backends.

Within that lifecycle, everything that *puts a file into a library* is an
**acquisition provider** — a Usenet downloader, a torrent downloader, or a store
you legitimately purchased from. The *arr apps are not a separate world beside
this pipeline; they become **requesters** on it. "Sonarr acquires a TV episode
via a downloader" and "a buy-a-TV-show provider acquires the same episode" are
the **same operation through the same pipeline** — different provider, identical
seams.

This is the `CAPABILITY-REGISTRIES.md` pattern applied to media: core holds the
abstractions (an `AcquisitionSource` trait + a media-management/metadata trait +
registries + the pipeline engine); every concrete source or backend — SABnzbd,
qBittorrent, Bandcamp, Libation, DriveThruComics, MakeMKV, and the metadata
providers — is an external plugin that registers in.

## The uniform lifecycle across media types

One verb surface; the media type selects the requester, the library path, and
the server to notify. Nothing below is type-specific logic in core — it is a
table of backends behind the same seams.

| Media | Requester(s) | Library path | Server to refresh | Metadata source |
|---|---|---|---|---|
| TV | sonarr | `/data/media/tv` | Plex/Jellyfin | TVDb / TMDb |
| Movies | radarr, radarr-4k | `/data/media/movies`, `/4k` | Plex/Jellyfin | TMDb |
| Music | lidarr | `/data/media/music` | Navidrome | MusicBrainz (beets) |
| Comics | mylar3 | `/data/media/comics` | Komga | ComicVine |
| Books | readarr / LazyLibrarian | `/data/media/books` | Calibre / Calibre-Web | Google Books / ISBN / OpenLibrary |
| Audiobooks | LazyLibrarian / Libation | `/data/media/audiobooks` | Audiobookshelf | Audible / OpenLibrary |

## Three decoupled roles

Acquisition is not one capability but three that compose:

| Role | Answers | Examples |
|---|---|---|
| **Requester / consumer** | *What* to get, by media identity + quality/format policy | sonarr (TV), radarr / radarr-4k (movies), lidarr (music), mylar3 (comics), readarr / LazyLibrarian (books) |
| **Acquisition provider** | *How* to get a DRM-free file | SAB (usenet), qBit (torrent), **and** purchased sources: Bandcamp, Libation/Audible, Libro.fm, DriveThruComics, itch.io, MakeMKV owned-disc, "buy this show" store |
| **Source catalog / indexer** | *Where* to find it | Prowlarr indexers (torznab/newznab); a store's own catalog is the same role for a purchased provider |

A requester resolves an identity (TVDB / TMDb / MBID / ComicVine / ISBN) and
hands a request to the pipeline. It does **not** care which provider fulfills
it. The pipeline picks an eligible provider under policy, the provider returns a
DRM-free file to staging, and the pipeline normalizes/tags it, files it into the
library (same NFS/SMB path the *arr apps already use), and triggers a
scan-on-import.

```
request(identity, policy)
      │
      ▼
 unified search ──► ranked hits across ALL providers (download + purchase)
      │
      ▼
 choose per hit:  acquire (download)   OR   purchase (buy DRM-free copy)
      │                                  │
      └──────────────┬───────────────────┘
                     ▼
        provider.acquire → DRM-free file in staging
                     ▼
        normalize / tag (beets, ComicVine, HandBrake, …)
                     ▼
        file into library  →  scan-on-import
```

## Unified search + acquire-vs-buy

Acquisition almost always begins with a **search**. There is **one** search
surface per media type that fans out across *every* provider — downloader
indexers **and** purchased-source catalogs — and returns a merged, ranked
result set keyed to a media identity.

For each hit, the user (or a standing policy) chooses **per result**:

- **acquire** — download it through a downloader provider, or
- **purchase** — buy the DRM-free copy through a purchase provider.

Both choices flow into the same `acquire → file → scan` tail. Default policy
should **prefer an already-owned / purchased copy** over a fresh download when
one is available, and may be configured per requester (e.g. "always try to buy
music if under $X, otherwise download").

## Per-user purchase accounts + mesh sharing

Downloading is a free, fleet-global action. **Purchasing spends money against a
real account**, so it carries an authorization model that downloading does not:

- **Purchasing is a per-user grant.** Not every mesh user may purchase. This is
  orca per-user authz, not a global toggle.
- **Each user brings their own account.** A user (e.g. `skey`) signs into a
  legitimate purchase source; that credential is bound to *that user's*
  identity and stored through orca secrets as an opaque reference — **never
  plaintext** (`docs/…` secrets model). `skey` purchases using `skey`'s account.
- **The account owner controls sharing, user-by-user.** `skey` may share the
  ability to purchase through `skey`'s source with specific other mesh users, or
  keep it private. Sharing is **opt-in and revocable**, granted per user — not
  all-or-nothing.
- **Anyone can add their own source.** Another user registers their own purchase
  account into the mesh and can purchase for the system; they likewise choose
  whether to share it.

The mesh therefore accumulates a **pool of purchase sources, each owned by a
user, each with its own sharing ACL**. A purchase request resolves:

```
purchase(item, requesting_user)
  → which registered accounts can fulfill this item's source/media type?
  → is requesting_user authorized on one of them (own account, or shared to them)?
      yes → execute purchase on that account, hand file to the pipeline
      no  → deny (optionally: route to an owner for approval)
```

This binds to mesh identity / SSO and orca's existing per-user authorization.
Downloader providers have no such ACL — they are shared infrastructure.

## The capability surface (verbs a provider implements)

Every provider — download or purchase — implements the same seam set. Not every
provider implements every verb (`search-catalog` only where a catalog exists):

| Verb | Downloader provider | Purchase provider |
|---|---|---|
| `authenticate` | client host + api key (SAB/qBit) | per-user: API key (DriveThru, itch), OAuth (Gumroad, Vimeo), session cookie (Humble, Bandcamp, Libro.fm), device registration (Libation, kobodl) |
| `search-catalog` | indexer search (torznab/newznab) | store catalog search (where exposed) |
| `list-owned` | n/a | enumerate the account's owned library |
| `acquire(item)` → DRM-free file | grab NZB/torrent → completed file | download owned/purchased item, de-DRM if required, emit EPUB/M4B/CBZ/MKV |
| `status` | queue/download progress | download/purchase/decrypt progress |

Files land in the same library paths and trigger the same scan-on-import
dispatcher the downloader stack already uses. The category/path matrix and
fleet-wide config convergence are unchanged from the downloader model — a
purchase provider is just another entry in the same define-once registry
(register the source/account **once**, reference it everywhere).

## Media management + metadata (a capability, not a side effect)

Acquisition is only half the job. Once a file is in the library, orca must be
able to **manage it and guarantee correct metadata** — this is a first-class
capability layered over the same services, not an afterthought bolted onto
downloading. It exposes uniform verbs across all media types:

| Verb | What it does | Backends |
|---|---|---|
| `identify(file/item)` | resolve a file or library item to a canonical id | ComicVine, MusicBrainz, TMDb/TVDb, ISBN/OpenLibrary |
| `refresh-metadata(item)` | (re)fetch correct tags/art/chapters and write them | beets, Mylar/ComicVine, *arr refresh, Calibre, Audiobookshelf |
| `organize(item)` | rename/relocate to the library's naming convention | beets, *arr, Mylar file-ops |
| `dedupe / reconcile` | detect duplicates and prefer owned/purchased over downloaded | pipeline policy |
| `verify` | confirm files are readable + match expected metadata | per-server scan + checksum |

Same platform shape: core holds the trait + registry; each concrete metadata or
management backend is a plugin. The requester asks for "correct metadata for
this item" and does not care which provider supplies it — identical to how it
asks for acquisition.

## What existing plugins must do

sonarr / radarr / radarr-4k / lidarr / mylar3 / LazyLibrarian must be
refactored to **request through this pipeline** rather than each carrying its
own bespoke download-client wiring. Their job narrows to: resolve identity +
quality/format policy, then dispatch a request. The pipeline owns provider
selection, acquire-vs-buy, filing, and scan.

---

## Appendix — DRM-free purchased-source research (2026-08-30)

Feasibility survey of legitimately-purchased, **DRM-free** (or de-DRM-able for
personal owned content) sources per media type, with the tool that already
implements the seams. **Owned-content only; no piracy tooling.** De-DRM sources
are flagged — they are DMCA §1201-gray (widely practiced for personal
format-shifting, not legally settled) and must sit behind an explicit per-user
opt-in. Every mainstream streaming/DRM store (iTunes, Amazon Prime, Netflix,
Disney+, ComiXology/Kindle, Movies Anywhere) is **excluded** — acquisition would
require DRM circumvention.

### Music
- **Bandcamp** — best target. DRM-free purchased downloads; tools: `bandcampsync`, `bandcamp-dl` (cookie/collection auth).
- **Qobuz** — FLAC purchases, DRM-free; second priority.
- Apple / Amazon Music / HDtracks / 7digital — DRM-free *files* but **no automatable owned-library API** → manual only.

### Books / eBooks
- ✅ **Standard Ebooks** — public-domain, official **OPDS** feed, no auth. Cleanest.
- ✅ **Humble Bundle** — DRM-free EPUB/PDF/CBZ; `/api/v1/order` JSON + `_simpleauth_sess` cookie; tool: `xtream1101/humblebundle-downloader`.
- ✅ **Tor / No Starch / Smashwords / à-la-carte O'Reilly** — DRM-free, session-cookie scrape of account page.
- ⚠️ **Kobo** (de-DRM) — `kobodl` covers ebooks **and** audiobooks via device registration; most attractive retail source.
- ⚠️ **Google Play Books** — export path; DRM-free if publisher allows, else ACSM→Adobe de-DRM (fragile).
- ⛔ **Amazon Kindle** — 2025 removed the USB-download path; effectively a DRM wall. Exclude. **O'Reilly Learning** subscription = streaming, exclude.

### Audiobooks
- ✅ **Audible** — already solved: **Libation** (CLI `LibationCli`) → DRM-free M4B. Model to replicate.
- ✅ **Libro.fm** — DRM-free M4B/MP3; tools: `burntcookie90/librofm-downloader` (Audiobookshelf-friendly), `libro-client`. Best after Audible.
- ✅ **Downpour** — DRM-free; `em-downpour-downloader`.
- ⚠️ **Kobo audiobooks** — same `kobodl` seam.

### Comics
- ✅ **DriveThruComics** — best target: official app-key API, native CBZ, incremental sync; tool: `drpg`.
- ✅ **itch.io** — official API key + `owned-keys` endpoint; implement natively.
- ✅ **Humble Bundle (comics)** — same downloader as books; CBZ/CBR.
- ⚠️ **Gumroad** — OAuth v2 works but buyer-library is weak (seller-centric); license-key / download-link ingestion.
- Partial (session-scrape, no API): **GlobalComix** (DRM-free PDF), **2000 AD** (native CBZ — great content).
- Watch: **Image via Sweet Shop** (DRM-free PDF, no API yet). Exclude: **Dark Horse Digital** (shut down Mar 2025), **ComiXology/Kindle** (DRM wall).

### TV / Movies (hardest — ~90% disc-ripping)
- ✅ **MakeMKV** (`makemkvcon`) — owned-disc rip → unencrypted MKV; flagship provider. First-class CLI, robot mode. Reference impl: **Automatic Ripping Machine** (udev disc-insert → MakeMKV → HandBrake → file to library). Transcode stage: **HandBrakeCLI**. 4K UHD needs a LibreDrive-compatible drive (surface in `detect-media`).
- ✅ **Internet Archive** — public-domain video; official `internetarchive` lib / `ia` CLI.
- Bespoke: **Gumroad / itch.io / Payhip** where a creator sells raw MP4/MKV — generic "download owned files" behavior.
- ⛔ All mainstream digital stores + streaming + library-streaming (Kanopy/Hoopla Widevine). **Vimeo On Demand shuts down Nov 2026** — do not build around it.

### Cross-cutting
- Register each source/account **once** in the define-once registry; reference everywhere.
- Identity mapping: purchased item → library metadata id (MBID / ISBN / ComicVine / TMDb / TVDB) for correct filing. Normalize toward the library-native format (prefer CBZ over PDF for Komga/Mylar; chapterized M4B for Audiobookshelf).
- Gate every de-DRM path behind an explicit per-user ownership affirmation; never redistribute; keep all repo language piracy-free.
