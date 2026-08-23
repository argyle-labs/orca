# Plugin registry: release-derived discovery + org manifests

Status: **planned** · Owner: TBD · Prompted by nut's first release (2026-08)

Replace the hand-curated `status` field in
`projects/system/src/plugin_catalog.json` with a **discovery** model. Flipping a
manual `unreleased → available` setting per plugin is the wrong pattern: the
catalog should reflect reality, and reality is "did the plugin publish a
release?" A plugin should declare its own published state; orca picks it up.

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

## Today (what's being replaced)

- `projects/system/src/plugin_catalog.json` — embedded via `include_str!` in
  `plugin_manager.rs` AND served as the canonical remote manifest from
  `raw.githubusercontent.com/argyle-labs/orca/main/...`. Runtime refresh
  supersedes the embedded copy (10-min TTL), so a manifest edit merged to `main`
  propagates to the fleet without an orca release.
- `status: available | unreleased | planned` — hand-set. `plugin.install --name`
  is refused unless `available`; `--file` sideloads regardless.
- `validate_catalog_entry` (`release_targets.rs`) is a pure schema check;
  `missing_release_assets` (release-asset completeness) exists but is a TODO —
  not wired to the network or a test.

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

## Interim

Until phase 1 lands, the canonical `plugin_catalog.json` is kept truthful by
hand (e.g. nut → `available` once it published `v0.0.1-rc.2`). This is
explicitly a stopgap, not the model.
