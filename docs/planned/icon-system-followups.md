# Icon system — planned follow-ups

Forward-looking extensions to the as-built icon system
([`../icon-system.md`](../icon-system.md)).

1. **Expose the icon through each plugin's metadata surface** so the UI / any
   client can fetch it (icon as a declared field on the plugin manifest /
   catalog entry, bound to the tool surface). Touches the plugin ABI /
   `orca-plugin.toml` / `plugin_catalog.json` — coordinate, do as its own slice.
2. **GitHub repo images** — set each repo's social-preview / README to its icon.
3. **Unraid Docker templates** — point container `<Icon>` URLs at the new assets.
