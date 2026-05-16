-- Web-UI account auth. Per project_rest_auth_v2.md:
--   * username UNIQUE is case-insensitive — username_lower is the canonical key.
--   * password_hash is argon2id (encoded form: "$argon2id$...").
--   * sessions slide on every authenticated request (last_used_at, expires_at refresh).

CREATE TABLE IF NOT EXISTS users (
    id                   TEXT PRIMARY KEY,
    username             TEXT NOT NULL,        -- display form, case preserved
    username_lower       TEXT NOT NULL UNIQUE, -- lookup key, lowercased
    password_hash        TEXT NOT NULL,
    role                 TEXT NOT NULL CHECK (role IN ('admin','member')),
    created_at           TEXT NOT NULL,
    password_updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at    TEXT NOT NULL,
    last_used_at  TEXT NOT NULL,
    expires_at    TEXT NOT NULL,
    revoked_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_user_active
    ON sessions(user_id, expires_at) WHERE revoked_at IS NULL;
