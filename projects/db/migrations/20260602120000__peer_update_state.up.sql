-- Per-peer update probe results. Populated by the background `peer_update_probe`
-- periodic in pod; read by `pod.list` so each peer's drawer/card shows that
-- peer's version/channel/pin/update — not the local daemon's.
--
-- The probe sends `system.update {}` (read-only, no args). Failures keep the
-- last good values in place; only `checked_at` advances on success.
CREATE TABLE IF NOT EXISTS peer_update_state (
    peer_id          TEXT PRIMARY KEY,
    version          TEXT,
    channel          TEXT,
    pinned_to        TEXT,
    latest           TEXT,
    update_available INTEGER NOT NULL DEFAULT 0,
    checked_at       INTEGER
);
