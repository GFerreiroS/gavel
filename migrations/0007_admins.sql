-- Who may change what the tracker collects.
--
-- Authorization until now was per *node* -- cluster roles -- and there was no
-- notion of one user being allowed more than another. Collecting is now a
-- runtime decision (which realms, which categories), so it needs a gate, and
-- the smallest honest gate is a flag on the user.
--
-- The first account to register becomes an admin. On a single-server instance
-- that is the person who deployed it; there is nobody else yet, and an
-- instance whose settings no one can reach would need a database edit to fix.
ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0;

-- What the tracker collects, as switches an admin can flip.
--
-- A row per thing that can be turned off. Absent means on: a category added by
-- a later release starts collected rather than silently ignored because
-- nobody had a row for it.
CREATE TABLE collection_settings (
    name    TEXT    NOT NULL PRIMARY KEY,
    enabled INTEGER NOT NULL
) WITHOUT ROWID;
