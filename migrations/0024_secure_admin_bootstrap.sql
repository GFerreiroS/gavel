-- Public registration must never grant administrative privileges. Existing
-- administrators are retained for upgrade compatibility; future bootstrap is
-- explicit and the repository performs its no-admin check atomically.
--
CREATE TABLE admin_bootstrap (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    -- NULL after the original administrator deletes their account. The row
    -- records that bootstrap was consumed, so deleting personal data cannot
    -- silently reopen the privileged bootstrap path.
    user_id   INTEGER REFERENCES users(id) ON DELETE SET NULL
) WITHOUT ROWID;

-- Preserve an administrator from an upgraded database without revoking any
-- other deliberately granted administrators.
INSERT INTO admin_bootstrap(singleton, user_id)
SELECT 1, MIN(id) FROM users WHERE is_admin = 1
HAVING MIN(id) IS NOT NULL;
