-- Revert the Route/Routes column-name cleanup back to the pre-cleanup names.
ALTER TABLE host_addressing  RENAME COLUMN last_seen_at TO detected_at;
ALTER TABLE host_addressing  RENAME COLUMN kind         TO key;
ALTER TABLE endpoints        RENAME COLUMN routes       TO addresses;
