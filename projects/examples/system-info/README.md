# system-info — orca example plugin (Rust)

Publishes a `SystemSnapshot` TypedValue (CPU load, memory, uptime) into the
`system:metrics` context every 5 seconds.

## Build

```sh
cargo build --release -p orca-example-system-info
```

The binary lands at `target/release/orca-example-system-info`. The bundled
`orca-plugin.toml` already points there, so once the plugin id is registered
the host can manage the lifecycle.

## Run standalone (against a dev host)

```sh
orca pki issue system-info
ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
ORCA_PKI_DIR=$HOME/.orca/pki \
ORCA_PLUGIN_ID=system-info \
target/release/orca-example-system-info
```

Subscribers see TypedValues like:

```json
{
  "type": "system-info.SystemSnapshot",
  "schema_version": "0.1.0",
  "sensitivity": "general",
  "payload": {
    "host": "mint.local",
    "uptime_sec": 184392,
    "load_1m": 1.23, "load_5m": 1.10, "load_15m": 0.95,
    "memory_total_kb": 16777216, "memory_used_kb": 8388608,
    "cpu_count": 12
  }
}
```
