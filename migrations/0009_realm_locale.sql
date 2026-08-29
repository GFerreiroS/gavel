-- What language a realm is played in.
--
-- EU is not one market to a reader: it is seven languages sharing a region,
-- and a list of ninety-two connected realms in one alphabetical run is
-- unreadable to someone looking for their own. Blizzard publishes a locale per
-- realm; storing it lets the admin page group by language instead.
--
-- Empty means unknown, which is what a realm recorded before this column
-- existed carries until the next startup refreshes it.
ALTER TABLE realms ADD COLUMN locale TEXT NOT NULL DEFAULT '';
