-- Recreate the peer-telemetry mirror tables (schema restored from
-- 20260602120000__peer_update_state and 20260602180000__peer_detail_state).
-- These are caches with no durable value; a rollback leaves them empty and the
-- (now-removed) pollers would have to be restored to repopulate them.
CREATE TABLE IF NOT EXISTS peer_update_state (
    peer_id          TEXT PRIMARY KEY,
    version          TEXT,
    channel          TEXT,
    pinned_to        TEXT,
    latest           TEXT,
    update_available INTEGER NOT NULL DEFAULT 0,
    checked_at       INTEGER
);
CREATE TABLE IF NOT EXISTS peer_detail_state (
    peer_id    TEXT PRIMARY KEY,
    payload    TEXT NOT NULL,
    checked_at INTEGER NOT NULL
);
