# `jellyfin.*` — Jellyfin media-server plugin

Manages registered Jellyfin endpoints and diagnoses server / library / transcode health through them.

Every tool surfaces identically on three transports — **CLI**, **MCP**, and **REST** — generated from the same `#[orca_tool]` declaration.

## Endpoint registry vs server diagnosis

Jellyfin follows the REST verb convention ([[feedback-rest-verbs-for-tool-surfaces]]):

| Verb | Semantics | Errors if |
|---|---|---|
| `jellyfin.list` | GET registered endpoints | — |
| `jellyfin.detail` | GET one endpoint | not registered |
| `jellyfin.create` | POST a new endpoint | name already exists |
| `jellyfin.update` | PATCH an endpoint | not registered |
| `jellyfin.delete` | DELETE an endpoint | — (idempotent) |

Diagnosis tools (`server_info`, `libraries`, `transcode_health`) take an `endpoint` name and call the registered server over HTTP.

---

## `jellyfin.create` — register a new endpoint

```sh
orca jellyfin create --name media --base-url http://127.0.0.1:8096 --token <api-token>
```

The token is the Jellyfin API key; it is sent as `Authorization: MediaBrowser Token="<token>"`.

---

## `jellyfin.server_info` — server name / version / OS

```sh
orca jellyfin server_info --endpoint media
```

### Output

```json
{ "serverName": "media", "version": "10.9.0", "operatingSystem": "Linux", "id": "…" }
```

---

## `jellyfin.libraries` — configured libraries

```sh
orca jellyfin libraries --endpoint media
```

### Output

```json
{ "libraries": [ { "name": "Movies", "collectionType": "movies", "locations": ["/data/movies"], "itemId": "…" } ] }
```

---

## `jellyfin.transcode_health` — **core diagnosis**

Classifies every active `/Sessions` entry as direct-play, hardware transcode, or software (CPU) fallback. A transcoding session whose `TranscodingInfo.HardwareAccelerationType` is `none` or absent is a **software fallback** — the signal that hardware acceleration is not engaging.

```sh
orca jellyfin transcode_health --endpoint media
```

### Output

```json
{
  "sessionCount": 3,
  "transcodingCount": 2,
  "softwareFallbackCount": 1,
  "anySoftwareFallback": true,
  "sessions": [
    {
      "sessionId": "a", "userName": "scott", "client": "Web", "deviceName": "…",
      "nowPlaying": "Film", "isTranscoding": true,
      "hardwareAccelerationType": null, "softwareFallback": true,
      "isVideoDirect": false, "videoCodec": "h264", "audioCodec": "aac",
      "transcodeReasons": ["VideoCodecNotSupported"]
    }
  ]
}
```

Branch on `anySoftwareFallback` to alert when HW accel stops engaging.

---

## Cross-transport invariants

- **Same args, same output, same errors** across CLI, MCP, REST.
- **REST endpoint:** every tool is at `POST /api/v1/<tool-name>` with the args as the JSON body.
- **CLI:** every tool is `orca jellyfin <verb> [args]`.

If parity breaks, that's a bug in the `#[orca_tool]` macro emission — not a per-plugin concern.
