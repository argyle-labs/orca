-- Phase C (CONTRACT) of the v7-id program for `config_rows`: `id` becomes the
-- row's own `uuidv7`, the value Phase A minted and the fleet has been converging
-- since 20260803000028.
--
-- Until now `id` was `noun:name@host_owner` — a string rebuilt from three columns
-- that sit on the same row, while `UNIQUE (noun, name, host_owner)` already
-- enforced that key. Code keys writes on the natural key and carries the uuidv7
-- as the id, so ownership and addressing stop sharing one string: a row keeps its
-- id when its owner is restated.
--
-- History follows the row, so it is repointed FIRST, while the old ids are still
-- in place to join on.
UPDATE config_rows SET uuidv7 = uuidv7() WHERE uuidv7 IS NULL;

UPDATE config_history
   SET row_id = (SELECT c.uuidv7 FROM config_rows c WHERE c.id = config_history.row_id)
 WHERE row_id IN (SELECT id FROM config_rows);

UPDATE config_rows SET id = uuidv7 WHERE id <> uuidv7;
