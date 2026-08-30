-- Things that happened, and when.
--
-- docs/market-analysis.md §9: correlating market movement with the expansion
-- needs explicit, timestamped events rather than labels inferred later from
-- the shape of a chart. This is the record. Phase 8 builds the pre/post
-- comparisons on it; nothing reads it for that yet.
CREATE TABLE market_events (
    -- Deterministic where it can be: a patch release's id is derived from the
    -- catalogue and the patch, so re-seeding writes the same row rather than a
    -- second copy of it.
    id          TEXT    NOT NULL PRIMARY KEY,
    kind        TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    notes       TEXT,
    -- UTC, always. A local time here would be a different instant depending on
    -- who read it back.
    starts_at   INTEGER NOT NULL,
    -- Null for something still going on.
    ends_at     INTEGER,
    -- Scope. Every column null means "everything", which is the common case
    -- and needs no special row. `regions` is a JSON array because an event can
    -- apply to several and to none-meaning-all; the rest are single keys.
    regions     TEXT    NOT NULL DEFAULT '[]',
    expansion   TEXT,
    patch       TEXT,
    -- Stored apart from `patch` even where the content maps one to one (§8).
    tier        TEXT,
    category    TEXT,
    item_id     INTEGER,
    -- The narrowest scope: one market, in its canonical encoding.
    market_key  TEXT,
    provenance  TEXT    NOT NULL CHECK (provenance IN ('catalogue', 'calendar', 'administrator')),
    validation  TEXT    NOT NULL CHECK (validation IN ('unvalidated', 'validated', 'rejected')),
    visibility  TEXT    NOT NULL CHECK (visibility IN ('internal', 'public')),
    recorded_at INTEGER NOT NULL,
    -- An event must end after it starts, or the interval is not one.
    CHECK (ends_at IS NULL OR ends_at >= starts_at)
) WITHOUT ROWID;

-- Every read of this table is "what happened in this window", so that is what
-- the index leads with.
CREATE INDEX idx_market_events_window ON market_events(starts_at DESC);
CREATE INDEX idx_market_events_kind ON market_events(kind, starts_at DESC);
