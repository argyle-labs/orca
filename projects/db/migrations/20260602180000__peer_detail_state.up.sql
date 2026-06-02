-- Per-peer `system.detail {}` probe results. Populated by the background
-- `peer_detail_probe` periodic in pod; read by `pod.list` so each peer's drawer
-- is hydrated from a fresh cached snapshot rather than an on-open RPC.
--
-- The probe sends `system.detail {}` (read-only, no args). The full
-- SystemStatusReport is stored verbatim as JSON so the consumer can pull any
-- field shape the UI grows to depend on (system / channels / daemon / etc).
-- Failures keep the last good row in place; only `checked_at` advances on
-- success.
CREATE TABLE IF NOT EXISTS peer_detail_state (
    peer_id     TEXT PRIMARY KEY,
    payload     TEXT NOT NULL,
    checked_at  INTEGER NOT NULL
);
