CREATE TEMP TABLE replication_ops_entity_map (old TEXT PRIMARY KEY, new TEXT NOT NULL);
INSERT INTO replication_ops_entity_map (old, new) VALUES
    ('pod_peers', 'mesh_peers'),
    ('pod_peer_addresses', 'mesh_peer_addresses'),
    ('pod_trust', 'mesh_trust'),
    ('pod_self', 'mesh_self'),
    ('pod_discovery', 'mesh_discovery'),
    ('pod_pending_offers', 'mesh_pending_offers');

DELETE FROM replication_ops
 WHERE entity IN (SELECT old FROM replication_ops_entity_map)
   AND EXISTS (
       SELECT 1 FROM replication_ops AS cur
         JOIN replication_ops_entity_map AS m ON m.new = cur.entity
        WHERE m.old = replication_ops.entity
          AND cur.key_val = replication_ops.key_val
          AND (cur.stamp_ms > replication_ops.stamp_ms
               OR (cur.stamp_ms = replication_ops.stamp_ms AND cur.op_id >= replication_ops.op_id)));

DELETE FROM replication_ops
 WHERE entity IN (SELECT new FROM replication_ops_entity_map)
   AND EXISTS (
       SELECT 1 FROM replication_ops AS old_op
         JOIN replication_ops_entity_map AS m ON m.old = old_op.entity
        WHERE m.new = replication_ops.entity
          AND old_op.key_val = replication_ops.key_val);

UPDATE replication_ops
   SET entity = (SELECT new FROM replication_ops_entity_map WHERE old = replication_ops.entity)
 WHERE entity IN (SELECT old FROM replication_ops_entity_map);

DROP TABLE replication_ops_entity_map;
