-- Ranks mirror contract::roles::role_rank. api_tokens.role is only
-- 'admin' or 'read', and no token tier matches 'user', so outranking admin
-- tokens are deleted rather than silently rewritten to 'read'.
CREATE TRIGGER IF NOT EXISTS users_demotion_revokes_credentials
    AFTER UPDATE OF role ON users
    WHEN (CASE new.role WHEN 'super-admin' THEN 4 WHEN 'admin' THEN 3
                        WHEN 'user' THEN 2 WHEN 'member' THEN 2
                        WHEN 'read-only' THEN 1 WHEN 'read' THEN 1 ELSE 0 END)
       < (CASE old.role WHEN 'super-admin' THEN 4 WHEN 'admin' THEN 3
                        WHEN 'user' THEN 2 WHEN 'member' THEN 2
                        WHEN 'read-only' THEN 1 WHEN 'read' THEN 1 ELSE 0 END)
    BEGIN
        DELETE FROM api_tokens
         WHERE user_id = new.id
           AND role = 'admin'
           AND new.role NOT IN ('super-admin', 'admin');
        UPDATE sessions
           SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE user_id = new.id AND revoked_at IS NULL;
    END;
