-- Dismissable (stateful) notifications.
--
-- The second notification plane. The first — `notifications::emit(Event)` —
-- is EPHEMERAL: it fans an event out to backends (ntfy/slack) and keeps no
-- state. This table is the STATEFUL plane: notifications that persist with a
-- lifecycle (active -> dismissed / suppressed), are ingested from many
-- sources (external systems + orca's own diagnostics), are classified by
-- audience (user vs system), and can be dismissed both locally and — later —
-- at their source.
--
-- `key` is a stable dedup id (e.g. `unraid:<host>:<src_id>`,
-- `diag:<provider>:<finding_id>`). Re-raising the same key is an idempotent
-- upsert that reactivates the row; raising a `suppressed` key is a no-op
-- (that is what "ignore permanently" means).
--
-- All timestamps are unix milliseconds (orca represents every time value in ms).
CREATE TABLE IF NOT EXISTS dismissable_notifications (
    key         TEXT PRIMARY KEY,
    -- Origin of the notification, e.g. `unraid@<host>`, `diagnostics:proxmox`.
    source      TEXT NOT NULL,
    -- The source's own id for this notification, used to dismiss it back at the
    -- source (e.g. an unraid notification id). NULL when the source has no
    -- addressable id or cannot be dismissed remotely.
    source_ref  TEXT,
    -- `info` | `warn` | `error` | `critical`.
    severity    TEXT NOT NULL,
    -- Whether the user can act on this (drives audience + surfaces a fix link).
    actionable  INTEGER NOT NULL DEFAULT 0,
    -- Optional remediation, JSON: external URL and/or in-orca deep link
    -- (provider + repair_id, or unit + action). NULL when not fixable.
    fix         TEXT,
    title       TEXT NOT NULL,
    body        TEXT,
    -- `user` | `system`. Derived on raise: user iff severity>=error OR actionable.
    audience    TEXT NOT NULL,
    -- `active` | `dismissed` | `suppressed`.
    state       TEXT NOT NULL DEFAULT 'active',
    -- Optional user targeting; NULL = not targeted at a specific user.
    user_id     TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- List queries filter by state and audience and order by recency.
CREATE INDEX IF NOT EXISTS idx_dismissable_notifications_state_audience
    ON dismissable_notifications (state, audience, updated_at);
