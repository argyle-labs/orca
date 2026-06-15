-- Per-host capability registry. One row per provider (docker, proxmox,
-- unraid, ...) recording whether THIS host can talk to that provider.
--
-- Populated at daemon startup by `system::capability::probe_all_capabilities`
-- and on operator-driven `system.capability.recheck`. Read by collectors and
-- tool surfaces to skip absent providers silently — no warn-every-tick spam
-- when (e.g.) docker isn't installed.
--
-- `state` semantics:
--   * `available` — probe succeeded; collectors and tools run normally
--   * `absent`    — probe failed (binary missing, marker file absent, etc.)
--   * `disabled`  — operator forced off via `system.capability.disable`;
--                   sticky across daemon restarts. probe_all_capabilities
--                   leaves disabled rows alone.
--
-- `reason` carries the failure/disable message; `detail` carries the version
-- string when available.
CREATE TABLE IF NOT EXISTS host_capabilities (
    provider     TEXT PRIMARY KEY,
    state        TEXT NOT NULL,
    last_probed  INTEGER NOT NULL,
    reason       TEXT,
    detail       TEXT
);
