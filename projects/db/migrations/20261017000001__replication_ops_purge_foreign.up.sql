BEGIN;
DELETE FROM replication_ops WHERE local_origin = 0 AND entity <> 'config_rows';
COMMIT;
