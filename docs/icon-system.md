# argyle-labs icon system

Every project (orca + each plugin) ships an icon as portable metadata that lives
in its own repo. One house style ties the whole roster together.

## House style

- **Tile**: 512×512, rounded-square (`rx=108`), inset 16px.
- **Background**: argyle harlequin lattice (two diamonds + dashed crosshatch) in
  **that project's brand colors** — e.g. plex amber/black, jellyfin purple/teal,
  homeassistant blue, sonarr blue, lidarr green, orca navy/teal.
- **Frame + Mark**: a single rounded border and the mark treatment, both keyed
  to the mark class:

| Mark class | Frame | Mark treatment | Examples |
|---|---|---|---|
| **Wrapped third-party services** | The teal (`#2BD3D3`) house frame — the shared argyle-labs signature that ties the wrapped roster together (home assistant is the external bar). | The **official logo** (never a hand-recreation), placed **large directly on the argyle** — no white inset plate behind it. **Hard rule: anything that has an official logo uses that logo, unmodified — never a recreation.** When the horizontal wordmark is a dark, light-background lockup that would vanish on the dark tile (e.g. zwave-js-ui), use the project's official **mark / app-icon variant** instead (its favicon / PWA icon / `logo.svg` — for zwave-js-ui, the blue hexagon Z). Do not recreate it, and do not box it. | plex app chevron, jellyfin (Wikimedia CC BY-SA), proxmox, home-assistant, docker, ntfy, unraid, immich (multicolor lens), and the Servarr logos (sonarr/radarr/prowlarr/lidarr/readarr/bazarr), dockge. |
| **Self-authored argyle-labs marks** | An **accent-matched frame** — the border takes the mark's own theme accent, the way raccoon and beaver do. raccoon + beaver are the internal fidelity bar. | The raccoon/beaver line-art style — a bold accent fill with dark outlines, high contrast, sized to fill the tile. | The animal marks (raccoon, beaver, walrus, …) and the glyph icons where no clean official logo exists (nfs share-folder, smb networked monitor, s3 bucket, mcp plug, agents orchestrator graph, whisper mic, db cylinder). |

  **orca itself**: two killer whales (eye-patch + dorsal fin) interlocked as a
  yin-yang.

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

Tiles are stamped ad-hoc from official source logos (Simple Icons / Servarr /
project repos) into the SVG source, then rendered to the PNG sizes below. The
reproducible facts are the *house style* rules above and the *layout* below —
the source of truth for any tile is its committed `assets/icon.svg` (orca's own
mark lives in
[`assets/branding/`](../assets/branding/): `orca-icon-a-argyle-tile.svg` plus
512/256 PNGs). Re-render from the SVG after a logo updates.

## Pending (next phase)

1. **Expose the icon through each plugin's metadata surface** so the UI / any
   client can fetch it (icon as a declared field on the plugin manifest /
   catalog entry, bound to the tool surface). Touches the plugin ABI /
   `orca-plugin.toml` / `plugin_catalog.json` — coordinate, do as its own slice.
2. **GitHub repo images** — set each repo's social-preview / README to its icon.
3. **Unraid Docker templates** — point container `<Icon>` URLs at the new assets.
