ALTER TABLE replication_ops ADD COLUMN local_origin INTEGER NOT NULL DEFAULT 0;
UPDATE replication_ops SET local_origin = 1
 WHERE origin = COALESCE((SELECT value FROM settings WHERE key = 'host.display_name'), '');
DELETE FROM replication_ops WHERE local_origin = 0 AND entity <> 'config_rows';
