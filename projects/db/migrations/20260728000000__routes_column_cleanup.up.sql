-- Route/Routes model cleanup (follows the WS2 convergence, PR #192).
--
-- After addressing converged onto the shared `Route` type, the on-disk column
-- names were left inconsistent with the model. This migration renames them so
-- the runtime schema is clean and consistent everywhere:
--   * endpoints.addresses      -> endpoints.routes
--     (the built-in endpoint_resource column is now `routes: Routes`; the
--     shared table MUST match or shared-mode SELECT/INSERT reference a column
--     that does not exist.)
--   * host_addressing.key         -> host_addressing.kind
--   * host_addressing.detected_at -> host_addressing.last_seen_at
--     (align with `pod_peer_addresses(kind, …, last_seen_at)` and `Route`.)
--
-- `apply_schema` runs BEFORE migrations and creates these tables with the OLD
-- names, and the earlier migrations (20260715 rebuilds host_addressing reading
-- `key`/`detected_at`; 20260725 seeds `endpoints.addresses`) also reference the
-- old names — so on every DB, fresh or fleet, the OLD columns exist by the time
-- this runs. `ALTER TABLE ... RENAME COLUMN` (SQLite >= 3.25) preserves data +
-- PK, so no backfill is needed.
ALTER TABLE endpoints        RENAME COLUMN addresses   TO routes;
ALTER TABLE host_addressing  RENAME COLUMN key         TO kind;
ALTER TABLE host_addressing  RENAME COLUMN detected_at TO last_seen_at;
