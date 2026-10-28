-- Widens the role CHECK beyond ('admin','member'). SQLite can't ALTER a CHECK
-- constraint in place, so rebuild the table.
PRAGMA foreign_keys=OFF;

CREATE TABLE users_new (
    id                  TEXT PRIMARY KEY,
    username            TEXT NOT NULL,
    username_lower      TEXT NOT NULL UNIQUE,
    password_hash       TEXT NOT NULL,
    role                TEXT NOT NULL CHECK (role IN ('admin','member','read-only','read','user','super-admin')),
    created_at          TEXT NOT NULL,
    password_updated_at TEXT NOT NULL,
    updated_at          TEXT NOT NULL DEFAULT '1970-01-01T00:00:00Z',
    email               TEXT
);

INSERT INTO users_new
    SELECT id, username, username_lower, password_hash, role,
           created_at, password_updated_at, updated_at, email
    FROM users;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

-- DROP TABLE above also drops any trigger defined on it (20261022000000);
-- recreate it on the rebuilt table so deletes still revoke credentials.
CREATE TRIGGER IF NOT EXISTS users_delete_revokes_credentials
    BEFORE DELETE ON users BEGIN
        DELETE FROM api_tokens WHERE user_id = old.id;
        UPDATE sessions
           SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE user_id = old.id AND revoked_at IS NULL;
    END;

PRAGMA foreign_keys=ON;
