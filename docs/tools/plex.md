# `plex.*` — Plex Media Server plugin

Manages registered Plex endpoints and diagnoses server / library / transcode health through them.

Every tool surfaces identically on three transports — **CLI**, **MCP**, and **REST** — generated from the same `#[orca_tool]` declaration.

The typed transport client is codegenned by progenitor from the vendored Plex spec (`specs/plex.openapi.yaml`, OpenAPI 3.1.1). The 3.1 document is lowered to 3.0 by the toolkit's `openapi::lower_31` pass, then pruned to the driven endpoints before codegen — the first plugin to ride the 3.1 path.

## Endpoint registry vs server diagnosis

Plex follows the REST verb convention ([[feedback-rest-verbs-for-tool-surfaces]]):

| Verb | Semantics | Errors if |
|---|---|---|
| `plex.list` | GET registered endpoints | — |
| `plex.detail` | GET one endpoint | not registered |
| `plex.create` | POST a new endpoint | name already exists |
| `plex.update` | PATCH an endpoint | not registered |
| `plex.delete` | DELETE an endpoint | — (idempotent) |

Diagnosis tools (`server_info`, `libraries`, `transcode_health`) take an `endpoint` name and call the registered server over HTTP.

---

## `plex.create` — register a new endpoint

```sh
orca plex create --name media --base-url http://127.0.0.1:32400 --token <plex-token>
```

The token is the Plex auth token; it is sent as the `X-Plex-Token` header.

---

## `plex.server_info` — server name / version / platform

```sh
orca plex server_info --endpoint media
```

### Output

```json
{ "friendlyName": "media", "version": "1.40.0", "platform": "Linux", "platformVersion": "…", "machineIdentifier": "…" }
```

---

## `plex.libraries` — configured library sections

```sh
orca plex libraries --endpoint media
```

### Output

```json
{ "libraries": [ { "key": "1", "title": "Movies", "kind": "movie", "locations": ["/data/movies"] } ] }
```

---

## `plex.transcode_health` — **core diagnosis**

Classifies every active `/status/sessions` entry as direct-play, hardware transcode, or software (CPU) fallback. A session whose `videoDecision` is `transcode` but whose `TranscodeSession.transcodeHwFullPipeline` is `false` is a **software fallback** — the signal that hardware acceleration is not fully engaging (`transcodeHwDecoding` / `transcodeHwEncoding` are surfaced when the server reports them).

```sh
orca plex transcode_health --endpoint media
```

### Output

```json
{
  "sessionCount": 2,
  "transcodingCount": 2,
  "softwareFallbackCount": 1,
  "anySoftwareFallback": true,
  "sessions": [
    {
      "sessionKey": "1", "title": "Film", "user": "scott",
      "player": "Living Room", "product": "Plex for Apple TV",
      "isTranscoding": true, "videoDecision": "transcode", "audioDecision": "copy",
      "transcodeHwFullPipeline": false, "transcodeHwRequested": true,
      "transcodeHwDecoding": null, "transcodeHwEncoding": null,
      "softwareFallback": true
    }
  ]
}
```

Branch on `anySoftwareFallback` to alert when HW accel stops engaging.

---

## Cross-transport invariants

- **Same args, same output, same errors** across CLI, MCP, REST.
- **REST endpoint:** every tool is at `POST /api/v1/<tool-name>` with the args as the JSON body.
- **CLI:** every tool is `orca plex <verb> [args]`.

If parity breaks, that's a bug in the `#[orca_tool]` macro emission — not a per-plugin concern.
