-- Drop the eager peer-telemetry mirror tables. Under the data-classification
-- law, observed/telemetry state is NOT synced or mirrored — each node answers
-- live queries about itself and consumers fetch on demand with an in-memory TTL
-- cache (`pod::peer_info`). The `peer_detail_state` (system.detail probe) and
-- `peer_update_state` (system.update probe) tables were populated by periodic
-- pollers that have been removed; nothing reads them anymore.
DROP TABLE IF EXISTS peer_detail_state;
DROP TABLE IF EXISTS peer_update_state;
