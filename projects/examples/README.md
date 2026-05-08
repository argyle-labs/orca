# orca example plugins

Real, useful plugins built against each language SDK. Each plugin demonstrates
the contract end-to-end: parse a manifest, connect over mTLS, declare a
TypedValue, periodically publish into a context. Drop one of these onto a
running orca host and its dashboard surfaces a new live feed.

| Plugin | Language | Source API | What it publishes |
|---|---|---|---|
| `system-info` (Rust) | Rust SDK | local OS metrics | `cpu_load`, `memory` snapshots |
| `adguard-home` (Go) | Go SDK | [AdGuard Home REST API](https://github.com/AdguardTeam/AdGuardHome/wiki/API) | DNS query stats |
| `hackernews` (TypeScript) | TS SDK | [HN Firebase API](https://github.com/HackerNews/API) | Top story metadata |
| `github-releases` (Kotlin) | Kotlin SDK | [GitHub Releases API](https://docs.github.com/en/rest/releases/releases) | New release events |

## How a plugin gets wired in

1. **Manifest (`orca-plugin.toml`)** declares the plugin id, version,
   surfaces, and capabilities. The SDK's manifest parser validates it.
2. **mTLS bundle** lives at `~/.orca/pki/plugins/<id>/`. The host issues it
   via `orca pki issue <id>`; plugins only ever load it.
3. **Connect** with the SDK's transport, send `orca/hello`, declare types
   with `orca/types.declare`, then `orca/context.publish` whenever you have
   a fresh value. Subscribers get pushed `orca/context.event` notifications.
4. **Run** as a subprocess of the host (managed by the plugin host) or
   stand-alone for development against `ORCA_PLUGIN_ADDR=…`.

Each example is self-contained — its README shows the exact dev loop.
