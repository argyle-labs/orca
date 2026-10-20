CREATE TABLE IF NOT EXISTS schedule_owners (
    name       TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL,
    role       TEXT NOT NULL,
    row_sha256 TEXT NOT NULL
);
