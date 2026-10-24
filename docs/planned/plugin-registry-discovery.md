# Plugin registry: release-derived discovery + org manifests

Status: **planned** · Owner: TBD · Prompted by nut's first release (2026-08)

Phases 1 and 2 have landed for the first-party org; phase 3 (third-party
manifests) remains.

## Principles

1. **Plugins self-declare published state — via their release.** No
   hand-maintained per-plugin status. The signal is the plugin repo cutting a
   release whose assets match the daemon's target triple. (The current catalog
   header comment already says: *"verify against the repo — does it publish a
   release? — not against this field."* This makes the code do that.)
2. **The org is the canonical source, not a setting.** orca discovers the plugin
   set from an **org manifest** (membership: "these repos are plugins") and
   derives availability per plugin from that repo's releases. Two concerns,
   cleanly separated: *membership* (org) vs *availability* (the plugin's release).
3. **Unified consumption.** All org plugins are consumed the same way.
4. **Third-party libraries = same pattern, different org manifest.** Anyone can
   host a plugin library by exposing their org's manifest; pointing orca at it
   surfaces their plugins. `argyle-labs` is the **first-party default**.
5. **Trust + safety.** Adding a **third-party** manifest is untrusted remote
   code execution → it must be **loud to the user and gated behind 2FA**.
   Per-plugin **provenance** (which manifest it came from) is always visible;
   first-party vs third-party is never ambiguous.

## Today

- `projects/system/src/plugin_discovery.rs` lists the `argyle-labs` org on the
  active release source (`release_source_api`: Gitea, else GitHub) and keeps
  non-archived repos carrying the `plugin` topic.
- Each plugin repo's recent releases name its plugins: an asset
  `{bin}-v{ver}-{triple}` yields an entry `bin` (so `arr` yields sonarr, radarr,
  …). A repo with no such asset yields one entry named after the repo.
- `status` is derived: `available` (stable release with an asset for this
  daemon's triple), `prerelease` (only prereleases have one), `unreleased`.
- `docsUrl` is the repo website when set, else `{repoUrl}#readme`.
- Results are cached in-process for 10 minutes and persisted to
  `$ORCA_HOME/cache/plugin-catalog.json`, the fallback when discovery fails.

## Target design

- **Membership discovery:** prefer a self-declaring repo **topic** (e.g.
  `orca-plugin`) enumerated via the org API (Gitea-canonical; mirror to GitHub),
  so a plugin joins the library by tagging itself — no orca edit. Alternative: a
  curated org manifest file. Decide during design.
- **Availability = release-derived:** a plugin is installable when its repo has a
  release with an asset matching the daemon triple
  (`{bin}-v{ver}-{triple}`, a bare executable — NOT `.so/.dylib`; the current
  catalog comment's naming is stale from the cdylib era). Wire
  `missing_release_assets` to an authenticated asset-list call.
- **Manifests as first-class:** a trust store of manifest sources with
  provenance; `argyle-labs` built in as first-party. Adding another is
  2FA-gated + loud (see the auth/user-bound-token model).
- **Offline:** keep an embedded bootstrap catalog as the air-gapped fallback;
  cache discovery results with a TTL (as today).

## Phasing (proposed)

1. **Release-derived availability** for first-party: compute `available` from the
   repo having a matching release asset, replacing the hand-set `status` for
   argyle-labs plugins. (Removes the manual flip — the immediate anti-pattern.)
2. **Membership via topic:** enumerate argyle-labs plugin repos by topic instead
   of a static list; drop the embedded membership list to a bootstrap fallback.
3. **Multi-manifest + trust:** support additional org manifests, provenance, and
   the 2FA-gated + loud add flow for third-party libraries.
