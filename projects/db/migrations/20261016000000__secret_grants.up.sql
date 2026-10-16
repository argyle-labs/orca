CREATE TABLE IF NOT EXISTS secret_grants (
    holder      TEXT NOT NULL,
    secret_name TEXT NOT NULL,
    granted_by  TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (holder, secret_name)
);
