# Orca Plugin Ecosystem Architecture

## Plugin types

Orca supports two classes of plugin. Both speak MCP JSON-RPC 2.0 over stdio.
The difference is deployment model and what they talk to.

### Host plugins
Run on a specific infrastructure host. Probe local services at startup.
Tools reflect what's reachable on that host — connectors that fail to probe
are silently skipped.

Example: **meerkat** — deployed to willow via SSH stdio. Speaks to Proxmox,
Docker, Unraid, NFS/SMB, and other on-host services.

Transport: `ssh root@willow /usr/local/bin/meerkat --stdio`

### Service plugins
Run anywhere (local, any server, as a subprocess). Talk to application APIs
over HTTP. No host-specific probing — if the API is unreachable, the tool
returns an error rather than being unregistered.

Examples: **jaguar** (arr stack), **ibis** (media), **ferret** (download clients).

Transport: typically a local subprocess — `node /usr/local/lib/jaguar/index.js --stdio`

---

## Spec-first implementation

Every service plugin must be built against the service's published API spec.
Pull the spec before writing any connector code. Specs are the source of truth
for field names, types, required parameters, and pagination patterns.

Known specs:

| Service | Spec location |
|---------|--------------|
| Sonarr | `http://{host}:{port}/api/v3/openapi` |
| Radarr | `http://{host}:{port}/api/v3/openapi` |
| Lidarr | `http://{host}:{port}/api/v1/openapi` |
| Prowlarr | `http://{host}:{port}/api/v1/openapi` |
| Readarr | `http://{host}:{port}/api/v1/openapi` |
| Jellyfin | `http://{host}:{port}/api-docs/openapi.json` (or published at api.jellyfin.org) |
| Audiobookshelf | `http://{host}:{port}/api-docs` (Swagger UI) |
| Navidrome | OpenSubsonic API — `opensubsonic.org/docs/endpoints/` |
| Calibre-Web | No official OpenAPI — scrape or use Calibre's internal OPDS |
| Kavita | `http://{host}:{port}/api/swagger/index.html` |
| Komga | `http://{host}:{port}/v3/api-docs` |
| Immich | `http://{host}:{port}/api-docs` (published at immich.app/docs/api) |
| qBittorrent | No OpenAPI — documented at github.com/qbittorrent/qBittorrent/wiki |
| SABnzbd | No OpenAPI — documented at sabnzbd.org/wiki/configuration/api |

For services without OpenAPI specs, fetch the HTML docs and write a spec stub
before implementing. Tools must match actual field names from the spec —
no guessing.

---

## Planned service plugins

### jaguar (TypeScript) — arr stack

**Repo:** `scottdkey/jaguar`
**Runtime:** Node.js (single compiled JS via tsup)
**Spec:** Pull OpenAPI from each running instance at startup (`/api/v3/openapi`)

Handles: Sonarr, Radarr, Lidarr, Prowlarr, Bazarr, Readarr, Mylar3, Kapowarr.
All v3-compatible arr apps share the same API surface. One connector type.

Config:
```toml
[[server]]
name      = "sonarr"
type      = "arr"
address   = "http://10.10.10.x:8989"
token_env = "SONARR_API_KEY"

[[server]]
name      = "radarr"
type      = "arr"
address   = "http://10.10.10.x:7878"
token_env = "RADARR_API_KEY"

[[server]]
name      = "prowlarr"
type      = "arr"
address   = "http://10.10.10.x:9696"
token_env = "PROWLARR_API_KEY"
```

Tools (per instance, spec-verified against OpenAPI):
- `{name}.arr.health` — version and health check
- `{name}.arr.queue` — active download queue (progress, ETA, protocol)
- `{name}.arr.wanted` — missing / cutoff-unmet items
- `{name}.arr.history` — recent grabs, imports, failures
- `{name}.arr.search` — trigger automatic search for an item
- `{name}.arr.calendar` — upcoming releases (configurable day range)
- `{name}.arr.library` — series/movie/artist/book list with monitored state
- `{name}.arr.command` — send a named command (RefreshSeries, RssSync, etc.)
- `{name}.arr.rootfolder` — configured root folders with disk usage
- `{name}.arr.tag.list` / `.add` / `.remove`
- `{name}.arr.indexer.list` — indexers and sync status (Prowlarr)
- `{name}.arr.blocklist` — blocked releases

---

### ibis (Kotlin) — media and library servers

**Repo:** `scottdkey/ibis`
**Runtime:** JVM fat JAR (Java 17+) or GraalVM native image
**Spec:** Pull OpenAPI/Swagger at startup where available; OpenSubsonic for Navidrome

Unified tool surface across all media and library server types. Each server type
exposes what it can — sessions only makes sense for streaming servers, not ebook
managers. Tools that don't apply to a server type return a capability error.

Handles:
- **Jellyfin / Emby** — video/music streaming (`/api-docs/openapi.json`)
- **Plex** — video/music streaming (XML API, no OpenAPI; use plex.tv docs)
- **Audiobookshelf** — audiobooks and podcasts (Swagger at `/api-docs`)
- **Navidrome** — music streaming (OpenSubsonic API)
- **Calibre-Web** — ebook library (OPDS + custom REST; limited API)
- **Kavita** — manga/comics/ebooks (Swagger at `/api/swagger/index.html`)
- **Komga** — comics/manga (OpenAPI at `/v3/api-docs`)
- **Immich** — photo management (OpenAPI at `/api-docs`)

Unified tool surface:
- `{name}.media.sessions` — active playback/streaming sessions (streaming servers only)
- `{name}.media.libraries` — library list with item counts and last scan
- `{name}.media.scan` — trigger library scan (full or path-specific)
- `{name}.media.search` — search by title, author, year, genre, tag
- `{name}.media.item.list` — paginated item listing with filters
- `{name}.media.item.info` — detailed item metadata
- `{name}.media.activity` — recent plays/reads/downloads
- `{name}.media.users` — user list with last active (servers that support multi-user)
- `{name}.media.schedule` — scheduled tasks and last run status (where applicable)
- `{name}.media.stats` — library statistics (item count, duration, storage)

Config:
```toml
[[server]]
name      = "jellyfin"
type      = "jellyfin"
address   = "http://10.10.10.x:8096"
token_env = "JELLYFIN_API_KEY"

[[server]]
name      = "audiobookshelf"
type      = "audiobookshelf"
address   = "http://10.10.10.x:13378"
token_env = "ABS_API_KEY"

[[server]]
name      = "navidrome"
type      = "navidrome"
address   = "http://10.10.10.x:4533"
user_env  = "NAVIDROME_USER"
pass_env  = "NAVIDROME_PASS"

[[server]]
name      = "kavita"
type      = "kavita"
address   = "http://10.10.10.x:5000"
token_env = "KAVITA_API_KEY"

[[server]]
name      = "immich"
type      = "immich"
address   = "http://10.10.10.x:2283"
token_env = "IMMICH_API_KEY"
```

---

### ferret (TypeScript) — download clients

**Repo:** `scottdkey/ferret`
**Runtime:** Node.js (single compiled JS via tsup)
**Spec:** No OpenAPI for most clients; implement against documented APIs

Handles: qBittorrent, SABnzbd, NZBGet, Transmission.

Tools (per instance):
- `{name}.dl.queue` — active downloads with speed, ETA, category, ratio
- `{name}.dl.pause` / `.resume` — pause/resume one download or all
- `{name}.dl.delete` — remove download (optional data deletion)
- `{name}.dl.add` — add by URL, magnet, or NZB file path
- `{name}.dl.history` — completed downloads
- `{name}.dl.stats` — current speeds, session totals
- `{name}.dl.categories` — configured categories/labels
- `{name}.dl.speedlimit` — get/set global speed limit
- `{name}.dl.free` — free disk space on download directory

Config:
```toml
[[server]]
name     = "qbit"
type     = "qbittorrent"
address  = "http://10.10.10.x:8080"
user_env = "QBIT_USER"
pass_env = "QBIT_PASS"

[[server]]
name      = "sabnzbd"
type      = "sabnzbd"
address   = "http://10.10.10.x:8080"
token_env = "SABNZBD_API_KEY"
```

---

## How plugins absorb into Orca securely

1. **Plugin publishes a standard tool surface** — tools follow
   `{instance}.{domain}.{operation}`. Claude discovers them via `tools/list`
   without knowing which plugin or language implements them.

2. **Orca federates, not proxies** — aggregates all plugin tool lists, routes
   calls. Claude sees one flat namespace.

3. **Secrets stay with the plugin** — each plugin process reads its own env
   vars. Orca's config only specifies the transport (command/args).

4. **Context scoping** — each context declares which plugins are active.
   `tools/list` only returns tools for the active context. A rebuy session
   never sees jaguar's arr tools.

5. **Plugin updates are independent** — `orca plugin update jaguar` restarts
   just that process. No Orca restart.

6. **Failure is isolated** — if ibis is unreachable, only media tools disappear.
   Everything else continues.

7. **Spec validation at startup** — service plugins fetch the API spec from
   the configured server at startup and validate their tool schemas against it.
   If the spec has changed (e.g. server was upgraded), the plugin logs a warning
   and falls back to its bundled spec version.

---

## Implementation order

1. Deploy meerkat to willow and validate (current focus)
2. **jaguar** (TypeScript, arr stack) — most frequently queried domain
3. **ferret** (TypeScript, download clients) — can share jaguar's repo initially as a package
4. **ibis** (Kotlin, media) — lower urgency; interim: use meerkat's graphql connector for Jellyfin

---

## MCP reference implementation

Each plugin needs:
- Read JSON-RPC 2.0 from stdin, write to stdout (newline-delimited)
- Handle `initialize`, `ping`, `tools/list`, `tools/call`
- Silence on notifications (requests with no `id`)
- Errors as `{"error": {"code": -32603, "message": "..."}}`

Reference implementations:
- **Go** — `meerkat/internal/mcp/server.go`
- **TypeScript** — use `@modelcontextprotocol/sdk` (official Anthropic SDK)
- **Kotlin** — `io.modelcontextprotocol:kotlin-sdk` or implement protocol directly
