UPDATE replication_ops
   SET key_col = 'mesh_id'
 WHERE entity IN ('mesh_self', 'mesh_pending_offers')
   AND key_col = 'pod_id';
