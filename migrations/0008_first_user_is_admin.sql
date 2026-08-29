-- Rescue an instance that already had accounts when admins were introduced.
--
-- 0007 added `is_admin` defaulting to 0, and registration only grants it when
-- the table is empty. On a database that already had users, that combination
-- leaves *nobody* able to reach /admin and no way to change it from inside the
-- app -- the settings would need a database edit to reach, which is exactly
-- what putting them behind a page was meant to avoid.
--
-- The oldest account becomes the administrator, matching the rule for a fresh
-- install: the first person here is the one who set it up. A no-op where the
-- table is empty, or where somebody is already an admin.
UPDATE users
   SET is_admin = 1
 WHERE id = (SELECT MIN(id) FROM users)
   AND NOT EXISTS (SELECT 1 FROM users WHERE is_admin = 1);
