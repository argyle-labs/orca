-- Rebuild the derived id from the columns it was concatenated from, history first.
UPDATE config_history
   SET row_id = (
        SELECT c.noun || ':' || c.name || '@' || c.host_owner
          FROM config_rows c WHERE c.id = config_history.row_id)
 WHERE row_id IN (SELECT id FROM config_rows);

UPDATE config_rows SET id = noun || ':' || name || '@' || host_owner;
