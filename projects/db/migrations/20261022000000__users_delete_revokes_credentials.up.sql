CREATE TRIGGER IF NOT EXISTS users_delete_revokes_credentials
    BEFORE DELETE ON users BEGIN
        DELETE FROM api_tokens WHERE user_id = old.id;
        UPDATE sessions
           SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE user_id = old.id AND revoked_at IS NULL;
    END;
