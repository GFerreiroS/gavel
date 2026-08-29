-- Which items a person actually cares about.
--
-- Alerts existed before this and were shown to everybody: the twenty most
-- recent, whoever you were, signed in or not. That is a feed, not an alert --
-- an alert is only an alert if it is about something you asked to be told
-- about. This table is that ask.
--
-- A watch is (person, item, region) and nothing else. What counts as "cheap"
-- stays where it was, in the collector's `AlertRule`, because the answer to
-- "is this unusually cheap" is a property of the market and not of who is
-- looking at it.
--
-- Region is part of the key on purpose: EU and US are separate markets, and
-- someone who plays on EU has no use for a US price.
CREATE TABLE user_watches (
    user_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id  INTEGER NOT NULL,
    region   TEXT    NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, item_id, region)
) WITHOUT ROWID;

-- The only read there is: everything one person follows.
CREATE INDEX idx_user_watches_user ON user_watches(user_id, added_at DESC);
