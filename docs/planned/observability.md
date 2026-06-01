# Observability — metrics, logs, viewer

Orca is the single pane of glass. Anything you'd open a browser for —
host CPU, container state, service logs, NFS health — opens in orca's
own UI first. External tools (Grafana, Loki, Prometheus) are
acceptable as offload destinations when the in-orca surface can't
keep up, but the **default answer is in-orca**: we minimize external
dependencies because every external dep is one more thing that has
to be installed, secured, and operated.

This doc supersedes the earlier "System Metrics & Tree UI" plan and
extends it with logs, retention, and the log-viewer surface.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Scope

Three pillars:

1. **Metrics** — numeric time-series sampled on a schedule.
   Local sysinfo, GPU, Docker stats, Proxmox/Unraid/UniFi APIs, etc.
2. **Logs** — line-oriented structured or unstructured text from
   orca itself, integrations, plugins, managed services, system
   journals, and managed-container stdout.
3. **Status / health** — discrete state (running / stopped /
   degraded), driven by probes. Existing in orca; this doc only
   touches it where it intersects with metrics/logs.

The fleet/host/VM/container hierarchy in §2 is shared across all
three pillars — same tree, three lenses.

The hierarchy shown below is **illustrative** (taken from the
scottkey/meerkat homelab). Orca builds the actual tree at runtime
from peer discovery + integration responses; nothing about the
shape is hard-coded.

---

## 2. System tree — hierarchy

The physical/virtual parent-child relationships that drive the tree UI:

```
fleet
├── speedy (MikroTik primary switch)
│   ├── thor (Proxmox node — 10.10.10.8)
│   │   ├── freyr (VM — Alpine Docker)
│   │   │   ├── sonarr, radarr, radarr4k, prowlarr, bazarr
│   │   │   ├── sabnzbd, qbittorrent, lidarr, mylar, kapowarr, lazylibrarian
│   │   │   └── [all other freyr containers]
│   │   ├── haos (VM — Home Assistant OS)
│   │   ├── pbs (VM — Proxmox Backup Server)
│   │   ├── mimir (LXC — Plex)
│   │   ├── adguard (LXC — AdGuard Home)
│   │   ├── unifi (LXC — UniFi controller)
│   │   ├── zigbee2mqtt (LXC)
│   │   └── zwave-js-ui (LXC)
│   ├── frigg (Proxmox node — 10.10.10.7)
│   │   ├── baldur (VM — Alpine Docker)
│   │   │   ├── caddy, ntfy, uptime-kuma, immich
│   │   │   ├── audiobookshelf, navidrome, kavita, komga, calibre
│   │   │   ├── dockge, syncthing
│   │   │   └── [all other baldur containers]
│   │   ├── maple (VM — Unraid secondary NAS)
│   │   │   └── NFS shares: data, backups
│   │   ├── njord (LXC — Plex, Intel QSV)
│   │   └── jellyfin (LXC — Jellyfin, Intel QSV)
│   ├── loki (Proxmox node — 10.10.10.9, router host)
│   │   └── opnsense (VM — router, WireGuard, DHCP)
│   ├── willow (Unraid primary NAS — 10.10.10.10)
│   │   ├── NFS shares: data, downloads, backups, halvor
│   │   └── Docker containers
│   └── mint (macOS — 10.10.10.40)
└── pedro (MikroTik media center switch — 10Gb uplink)
    ├── PS5
    ├── bedroom TV
    ├── soundbar
    └── Nintendo Switch
```

Parent/child relationships are established automatically when:
- A Proxmox node reports a VM/LXC via the Proxmox API → VM is a child of the node
- A Docker host reports containers → containers are children of the host
- A pod peer is discovered via mDNS on the same subnet as a known switch → associated with that switch
- Manual override: `orca system peer update --parent <host>` for cases the API can't infer

---

## 3. Metrics — what we collect

### 3.1 Rust native (via `sysinfo` crate) — runs on every orca host

| Metric | Detail |
|--------|--------|
| CPU usage | Per-core %, aggregate %, frequency, model name, core count |
| RAM | Used / total / available, swap used/total |
| Disk | Per-mount: used/total/available, read/write bytes per second |
| Network interfaces | Per-NIC: bytes sent/received, packets, errors, MTU, MAC, IP addresses |
| Network traffic | Per-NIC current throughput + rolling average (1m / 5m / 15m) |
| Processes | Top consumers by CPU and RAM, total process count |
| Load average | 1m / 5m / 15m |
| Uptime | Since last boot |
| System info | OS, kernel version, hostname |

### 3.2 GPU — rust native

| GPU vendor | Crate | Metrics |
|-----------|-------|---------|
| NVIDIA | `nvml-wrapper` | GPU utilization %, VRAM used/total, temperature, power draw, clock speeds, fan speed, encoder/decoder utilization |
| Intel (iGPU) | sysfs `/sys/class/drm/` + `intel-gpu-tools` | render engine %, video engine %, VRAM (shared), frequency |
| AMD | sysfs `/sys/class/drm/` + ROCm | GPU %, VRAM, temperature, power |

GPU presence detected at startup; collector activates only if hardware is present.

### 3.3 Proxmox API — for VMs and LXCs

Polled on each Proxmox node's orca instance, aggregated up to the tree:

| Metric | Detail |
|--------|--------|
| Node CPU | Usage %, core count, model |
| Node RAM | Used/total |
| Node storage | Per-pool (local-lvm, local, etc.) used/total |
| Node network | Per-interface throughput |
| Node temperature | If IPMI/sensors available |
| Per-VM/LXC CPU | % of allocated vCPUs |
| Per-VM/LXC RAM | Used/allocated |
| Per-VM/LXC disk I/O | Read/write bytes per second |
| Per-VM/LXC network | In/out bytes per second |
| Per-VM/LXC uptime | Running / stopped / paused state |

### 3.4 Unraid GraphQL API

| Metric | Detail |
|--------|--------|
| Array status | Started/stopped, parity state |
| Per-disk | Temp, SMART status, read/write bytes, spin state |
| Array throughput | Aggregate read/write MB/s |
| Cache pool | Used/total, type (SSD/NVMe) |
| Share usage | Per-share used/total |
| Docker containers | CPU%, memory, status (running/stopped) |
| Mover status | Active/idle, bytes moved |
| UPS | If connected: battery %, load %, input voltage |

### 3.5 Docker API

Per container:

| Metric | Detail |
|--------|--------|
| CPU | % usage (cgroup stats) |
| RAM | Used / limit / cache |
| Network I/O | Per-interface bytes in/out |
| Block I/O | Read/write bytes |
| Status | Running / stopped / restarting |
| Restart count | Crash loop indicator |

### 3.6 UniFi Network API — AP and client visibility

| Metric | Detail |
|--------|--------|
| Per-AP | Uptime, connected client count, channel, TX power, firmware version |
| Per-AP throughput | Bytes in/out per second |
| Per-client | Hostname, IP, MAC, SSID, VLAN, RSSI, TX/RX rate, bytes transferred, connection duration |
| Fleet summary | Total clients by VLAN, total throughput |

This is the **only** source of data for WiFi-only devices. Without it, those devices are invisible.

### 3.7 OPNsense API (future) — WAN and routing visibility

| Metric | Detail |
|--------|--------|
| WAN throughput | Bytes in/out per second on WAN interface |
| Active connections | State table size, top talkers |
| VPN tunnels | WireGuard peers, bytes transferred per tunnel |
| DHCP leases | Active leases by VLAN |
| Firewall hits | Top blocked IPs, rule hit counts |

### 3.8 Metric storage and retention

Every metric sample stored in SQLite at 15s resolution. Materialized
rollups: 1m, 5m, 15m, 1h averages.

| Resolution | Retention | Use |
|---|---|---|
| 15s raw | 24h | Live graphs, last-hour zoom |
| 1m rollup | 30 days | Default detail panel window |
| 1h rollup | 1 year | Long trends, capacity planning |

Retention is configurable per metric source via
`config/<host>/observability.toml`. Rollup tables are SQLite
materialized views; cleanup runs hourly.

---

## 4. Logs — collection, retention, viewer

This is the gap the previous plan didn't cover. Same in-orca-first
principle: logs land in orca's local store on each host, are
queryable across the mesh, and only get shipped to an external
system (Loki, Elasticsearch) if a specific use case demands it.

### 4.1 Sources

| Source | How orca picks it up |
|---|---|
| Orca daemon + plugins | Direct: orca writes structured logs to its own log sink. No file scraping. |
| systemd journal | `sd-journal` rust bindings on systemd hosts. Streams to local store. |
| OpenRC / Alpine | Tail of `/var/log/messages` + service-specific log files. |
| Managed Docker containers | `docker logs --follow` per container via the docker engine API (rust-native bollard). Container metadata (name, image, compose project) attached as fields. |
| LXC | Proxmox API `lxc.log` endpoint + journal-from-inside-LXC where reachable. |
| Caddy access logs | Caddy emits JSON; orca tails the log file (or receives via Caddy admin API). |
| Unraid syslog | Unraid GraphQL exposes recent log lines; polled. |
| OPNsense / routers | syslog forwarding to the local orca daemon's UDP/TCP syslog listener. |
| Plugins | Plugins log to stderr; plugin host captures and tags with `plugin=<id>`. |

All sources funnel into a unified log record:

```rust
struct LogRecord {
    ts: Timestamp,              // monotonic, in UTC
    host: PeerId,
    source: LogSource,          // daemon | systemd | docker | lxc | syslog | plugin | ...
    subject: String,            // unit name, container name, plugin id, etc.
    level: Level,               // trace..error, or unknown
    fields: BTreeMap<String, JsonValue>,
    message: String,
}
```

### 4.2 Storage

Per-host SQLite (separate database from metrics — write patterns
differ: logs are append-heavy, metrics are upsert-rollup).

Schema sketch:

```sql
CREATE TABLE log (
    ts          INTEGER NOT NULL,          -- microseconds since epoch
    source      TEXT NOT NULL,
    subject     TEXT NOT NULL,
    level       INTEGER NOT NULL,          -- 0=trace .. 4=error
    fields_json TEXT,                      -- structured fields
    message     TEXT NOT NULL
) STRICT;

CREATE INDEX log_ts ON log (ts);
CREATE INDEX log_source_subject_ts ON log (source, subject, ts);
-- FTS5 virtual table for full-text search on message + fields
```

Disk budget per host capped (default 2 GB). Approaching cap triggers
the retention policy in §4.3.

### 4.3 Retention policy (configurable, defaults shown)

| Tier | Default | Rationale |
|---|---|---|
| `error` + `warn` | 90 days | Long-term — these are the lines you want when chasing an incident later. |
| `info` | 14 days | Day-to-day operational context. |
| `debug` + `trace` | 24 hours | Useful while a problem is fresh; expensive long-term. |
| Managed container stdout | 7 days | The container itself is the source of truth; orca keeps a working window. |
| Audit events (auth, ACL, GitOps apply) | 1 year | Compliance / forensics. Cannot be lowered below 90 days even by config. |

Retention is enforced by a periodic job (hourly) that prunes by
`(level, age)`. Disk-pressure mode (over 90% of budget) accelerates
pruning starting from `debug`.

Per-source overrides via config:

```toml
[log.retention]
default.info = "14d"
default.debug = "24h"

[log.retention.source.docker]
# Keep more for containers we deem important
subject."immich-server".info = "30d"
subject."caddy".info = "60d"
```

### 4.4 Cross-mesh query

Logs are first stored locally on the host that produced them. Cross-host
queries fan out via the existing pod-mesh:

```
orca log query \
  --host any \
  --source docker \
  --subject 'sonarr|radarr|prowlarr' \
  --level warn+ \
  --since 1h \
  --grep 'connection refused'
```

Equivalent MCP tool: `log_query`. Same shape, structured args.

Results stream back as they arrive; no waiting for slowest peer.

### 4.5 Log viewer (in orca UI)

Lives in the same UI as the system tree. Two entry points:

1. **From the tree**: clicking any host/VM/container opens its
   detail panel; the panel has a "Logs" tab that pre-filters by
   that subject. Live tail by default; scrubbing back loads
   historical pages from SQLite.
2. **Global search**: free-form query bar with field shortcuts
   (`host:baldur source:docker level:warn+ since:6h`). Saved
   queries pinned to the sidebar.

UX shape:
- Virtualized list (handles 10k+ lines without choking).
- Each row: timestamp (relative + absolute on hover), level pill,
  source/subject chip, message (truncated, expandable). Structured
  fields shown inline for known schemas (HTTP status, request ID).
- Color by level. Filtering inline by clicking any chip.
- Time range picker shared with metrics graphs in the same panel —
  pan/zoom on the metric graph re-scopes the log feed.

### 4.6 External offload (optional)

Some users will want their logs in Loki/Elasticsearch/Grafana — for
team workflows or longer-than-1yr retention. Orca supports it as an
**output sink**, not a replacement for the local store:

| Sink | Crate / approach |
|---|---|
| Loki | HTTP push (rust `reqwest`); structured fields → Loki labels |
| Elasticsearch | bulk index API |
| OpenTelemetry collector | OTLP exporter |

Sinks are configured per-host:

```toml
[log.sink.loki]
url = "https://loki.example.com/loki/api/v1/push"
labels = { fleet = "homelab", host = "${HOSTNAME}" }
min_level = "info"
```

Local retention is unaffected. If the sink is unavailable, lines
buffer locally up to the sink's configured backlog (default 1 GB),
then drop oldest.

---

## 5. Collection architecture

```
Each orca host runs collectors locally:
  - sysinfo collector (always on, 15s interval)
  - GPU collector (activates if hardware present, 15s)
  - Docker collector (activates if Docker socket present, 15s)
  - Proxmox collector (activates if Proxmox API configured, 30s)
  - Unraid collector (activates if Unraid API configured, 30s)
  - UniFi collector (activates if UniFi API configured, 30s)
  - Log tailers (one per source detected on the host)

Storage (separate SQLite per concern):
  - metrics.db: raw + rollups (15s raw 24h, 1m 30d, 1h 1yr)
  - logs.db: append-heavy, retention per §4.3
  - audit.db: 1-year minimum, separate to prevent accidental wipe

Pod mesh: any peer can stream another peer's metrics or logs:
  metrics_stream  --host <name> --interval 15s
  metrics_history --host <name> --window 1h --resolution 1m
  log_tail        --host <name> --source docker --subject X
  log_query       --host <name> --since 1h --grep '...'

UI:
  - Live mode: WebSocket stream of 15s metric samples + new log lines
  - History load: paginated reads from SQLite
  - Time cursor synced across all panels in a detail view
```

---

## 6. UI — view modes

### 6.1 Tree view

Mirrors the physical/virtual hierarchy. Each row shows the node
name, a 15-minute CPU sparkline, RAM bar, network TX/RX sparkline,
and (new) a log-rate sparkline — all animated, updating every 15s.

```
▼ fleet
  ▼ thor  〜cpu〜 RAM 44%  〜net↑↓〜  〜log/s〜
    ▼ freyr VM   〜cpu〜  RAM 31%  〜net↑↓〜  〜log/s〜
        sonarr     ● running   〜log/s〜
        radarr     ● running   〜log/s〜
    ▷ haos VM    〜cpu〜  RAM 18%
  ▼ frigg   〜cpu〜  RAM 38%  〜net↑↓〜  〜log/s〜
```

Hovering the log-rate sparkline reveals warn/error counts; clicking
opens the logs tab pre-filtered to that subject.

### 6.2 Grouped view

Same data, reorganized by resource type across all hosts:

```
Proxmox Nodes    — thor, frigg, loki
Unraid NAS       — willow, maple
Docker Hosts     — baldur, freyr, willow
VMs / LXCs / Containers / Network / Storage / WiFi clients
```

### 6.3 Detail panel (any node)

Selecting any node opens a detail panel with:

- **Header**: name, IP, type badge, status pill, uptime.
- **CPU / RAM / GPU / Disk / Network**: animated area graphs as in
  the previous plan — live tail + historical scrollback, shared
  time cursor.
- **Logs tab**: live tail + filter chips + history scrollback.
  Time cursor synced with the metrics graphs above — drag a CPU
  spike, the log feed scrolls to that moment.
- **Children**: inline list of VMs/LXCs/containers.
- **Services**: catalog-registered services on this node.

Time window selector global to the panel: `15m` `1h` `6h` `24h` `7d`.

---

## 7. Alerting + retirement targets

Out of scope for the first cut, but the shape is clear: orca's
status surface already does discrete probes. Threshold alerts
(CPU > 90% for 5m, disk > 85%, log error rate > N/s) belong here
and should reuse the same ntfy integration that orca already has
in Prod.

### 7.1 Named retirement targets (parity rule applies)

Each of these stays in place until orca's observability surface
proves parity per [schema-evolution.md](schema-evolution.md):

| Target | Path | Successor surface |
|---|---|---|
| Uptime Kuma | `meerkat/compose/uptime-kuma/` | Threshold-alerts + status probes in this doc; ntfy integration already Prod |
| ntfy server (deployment) | `meerkat/compose/ntfy/` | `projects/plugins/ntfy` is the *client*; the *server* stays as a compose stack until storage-mesh reconciler owns it |
| Per-host NFS watchdog configs | `meerkat/scripts/{baldur,freyr,thor,pbs,frigg}/nfs-monitor.conf` | `projects/plugins/nfs` health probes + [storage-shares.md](storage-shares.md) client reconciler |

The system tree + collector seed already exists in
`projects/system/src/topology/` (`mod.rs`, `proxmox.rs`). Extend
that, don't greenfield.

### 7.2 Metrics storage — pick SQLite (embedded)

**Decision: embedded SQLite**, not an external TSDB (Prometheus, VictoriaMetrics, InfluxDB).

Reasoning:

- Orca already ships SQLite for the config store, secrets store,
  audit DB, sessions, scheduler-runs — adding a TSDB doubles the
  daemon-side dependency surface for one more concern.
- The "in-orca first, minimize external deps" rule in §1 of this
  doc applies most strongly to the layer that has to be up before
  you can debug anything *else*. A TSDB outage masks itself.
- The retention math fits: 15s raw × 24h is ~5700 samples/series;
  1m × 30d is ~43k; 1h × 1yr is ~8800. Even with 200 series per
  host and 10 hosts the working set is well inside SQLite's
  comfort zone with the materialized-rollup approach in §3.8.
- External offload to Loki/Prometheus/OTLP stays available as a
  **sink** (§4.6 for logs; same shape for metrics) for users who
  have a fleet-wide observability stack. The in-orca store is
  always the primary.

Schema: `metrics.db` separate from `logs.db` and `audit.db`
(different write patterns; see §4.2 logs rationale). Per-host;
cross-host queries fan out over pod mesh.

---

## 8. Implementation phases

| Phase | Item | Size |
|---|---|---|
| A | Local host metrics (sysinfo + GPU) — every orca host, per-host detail panel, no cross-host yet | M |
| B | Docker stats via bollard, containers in tree | M |
| C | Proxmox guest metrics, VM/LXC tree expansion | M |
| D | Unraid GraphQL metrics | M |
| E | UniFi client visibility | M |
| F | Cross-host metric aggregation via pod mesh + tree UI | L |
| L1 | Log record + per-host SQLite store + retention job | M |
| L2 | systemd-journal + docker-logs tailers, log_tail/log_query MCP tools | M |
| L3 | Log viewer UI: tree integration + detail-panel logs tab + global search | L |
| L4 | Plugin/audit/syslog tailers; Caddy access log parsing | M |
| L5 | External sinks (Loki/Elastic/OTLP) | M |
| G | OPNsense + network layer metrics | M |
| Alert | Threshold alerts + parity check vs Uptime Kuma → retire Kuma | M |

A-F can land independently. L1 unblocks L2-L4. L5 is opt-in.

---

## 9. Open questions

1. **Polling interval**: 15s for local metrics, 60s for API-based
   (Proxmox/Unraid/UniFi)? Configurable per source. Recommend
   keeping defaults conservative.
2. **Audit DB separation**: §4.2 puts logs in `logs.db` and audit
   events in `audit.db`. Worth a single DB with strict per-table
   retention instead? Lean separate — easier to back up audit
   independently per [backup-restore.md](backup-restore.md).
3. **Log format on the wire** for cross-mesh streams: msgpack vs
   JSON-lines? msgpack is smaller, JSON-lines is grep-able if you
   capture the wire. Lean msgpack with a `--debug-wire` flag that
   switches to JSON.
4. **In-orca alerting vs Grafana Alertmanager**: same trade as the
   sinks discussion. In-orca first; offload available.
5. **Push vs pull** for remote API sources: local sysinfo can push;
   Proxmox/Unraid/UniFi must pull. Keep both, normalize the storage
   layer.
