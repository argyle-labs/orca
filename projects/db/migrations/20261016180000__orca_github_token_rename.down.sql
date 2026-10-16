UPDATE secrets SET name = 'github_token'
WHERE name = 'orca.github_token'
  AND NOT EXISTS (SELECT 1 FROM secrets WHERE name = 'github_token');
UPDATE settings SET key = 'secrets.github_token'
WHERE key = 'secrets.orca.github_token'
  AND NOT EXISTS (SELECT 1 FROM settings WHERE key = 'secrets.github_token');
UPDATE secret_grants SET secret_name = 'github_token'
WHERE secret_name = 'orca.github_token'
  AND NOT EXISTS (
    SELECT 1 FROM secret_grants g
    WHERE g.holder = secret_grants.holder AND g.secret_name = 'github_token'
  );
