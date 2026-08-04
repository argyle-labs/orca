-- Recreate the retired controller-side telemetry mirror. Empty on restore; it
-- was repopulated by the (also-retired) background peer_detail probe.
CREATE TABLE IF NOT EXISTS peer_detail_state (
    peer_id     TEXT PRIMARY KEY,
    payload     TEXT NOT NULL,
    checked_at  INTEGER NOT NULL
);
