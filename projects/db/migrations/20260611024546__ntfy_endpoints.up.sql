-- ntfy endpoint registry. One row per registered ntfy server+topic. The ntfy
-- plugin reads enabled rows at startup and registers each as a backend with
-- the notifications dispatcher.

CREATE TABLE IF NOT EXISTS ntfy_endpoints (
    name       TEXT PRIMARY KEY,
    base_url   TEXT NOT NULL,
    topic      TEXT NOT NULL,
    token      TEXT,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
