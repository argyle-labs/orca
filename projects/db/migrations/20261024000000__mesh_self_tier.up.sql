ALTER TABLE mesh_self ADD COLUMN secure INTEGER NOT NULL DEFAULT 0;
CREATE TABLE mesh_self_tier (
    id                       INTEGER PRIMARY KEY CHECK (id = 1),
    secure                   INTEGER NOT NULL DEFAULT 0,
    mesh_id                  TEXT,
    ca_previous_expires_at   INTEGER,
    set_at                   INTEGER NOT NULL
);
INSERT INTO mesh_self_tier (id, secure, mesh_id, ca_previous_expires_at, set_at)
    SELECT id, 0, mesh_id, ca_previous_expires_at, set_at FROM mesh_self;
DROP TABLE mesh_self;
ALTER TABLE mesh_self_tier RENAME TO mesh_self;
