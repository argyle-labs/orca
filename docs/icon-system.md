# argyle-labs icon system

Every project (orca + each plugin) ships an icon as portable metadata that lives
in its own repo. One house style ties the whole roster together.

## House style

- **Tile**: 512×512, rounded-square (`rx=108`), inset 16px.
- **Background**: argyle harlequin lattice (two diamonds + dashed crosshatch) in
  **that project's brand colors** — e.g. plex amber/black, jellyfin purple/teal,
  homeassistant blue, sonarr blue, lidarr green, orca navy/teal.
- **Frame**: a single teal (`#2BD3D3`) rounded border — the constant
  argyle-labs/orca signature across every icon.
- **Mark**:
  - **Wrapped third-party services** use the **official logo** (never a
    hand-recreation): plex app chevron, jellyfin (Wikimedia CC BY-SA), proxmox,
    home-assistant, docker, ntfy, unraid, and the Servarr logos
    (sonarr/radarr/prowlarr/lidarr/readarr/bazarr), dockge.
  - **orca itself** is the only self-authored mark: two killer whales (eye-patch
    + dorsal fin) interlocked as a yin-yang.
  - Where no clean official logo exists, a simple argyle-labs glyph stands in
    (nfs/smb storage, mcp plug, db cylinder, agents node).

## Per-repo layout

Each repo carries the source SVG plus rendered PNGs in `assets/`:

```
<repo>/assets/
  icon.svg          # source of truth
  icon.png          # 512×512
  icon-256.png      # 256×256
```

The `arr` repo bundles several services, so it exposes one file per service
(`sonarr.svg`, `radarr.svg`, … plus `icon.svg` = the four-app quad) so the
plugin can surface a per-service icon.

In-tree core domains that carry assets (e.g. `db`, the `agents` domain at
`projects/agents`) keep them under `projects/<name>/assets/`. orca's own brand
mark lives in `assets/branding/`.

## Regenerating

`/tmp/gen_icons.py` + `/tmp/gen_icons2.py` stamp the tiles from official source
logos (Simple Icons / Servarr / project repos). Re-run after a logo updates.

## Pending (next phase)

1. **Expose the icon through each plugin's metadata surface** so the UI / any
   client can fetch it (icon as a declared field on the plugin manifest /
   catalog entry, bound to the tool surface). Touches the plugin ABI /
   `orca-plugin.toml` / `plugin_catalog.json` — coordinate, do as its own slice.
2. **GitHub repo images** — set each repo's social-preview / README to its icon.
3. **Unraid Docker templates** — point container `<Icon>` URLs at the new assets.
