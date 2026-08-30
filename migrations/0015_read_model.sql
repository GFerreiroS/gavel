-- The read model: what a page reads instead of reducing a history.
--
-- CLAUDE.md §15's performance rule. Collection and calculation are the write
-- path; HTTP is a read path. A request never reduces, never waits for a
-- worker, and never sees a row belonging to a version that is still being
-- built.

-- Each attempt at recalculating. One row per candidate, published or not.
CREATE TABLE analysis_versions (
    version      INTEGER PRIMARY KEY AUTOINCREMENT,
    state        TEXT    NOT NULL CHECK (state IN ('staging', 'published', 'failed')),
    -- Which rules produced it. Not the catalogue version: that says what the
    -- market was, this says how it was measured.
    algorithm    INTEGER NOT NULL,
    started_at   INTEGER NOT NULL,
    published_at INTEGER,
    -- The interval of source observations this version reduced, so a result
    -- can be reproduced or told apart from a rebuild.
    source_from  INTEGER,
    source_until INTEGER,
    markets      INTEGER NOT NULL DEFAULT 0,
    -- Why it failed, for the operations page. Null while it is alive.
    note         TEXT
);

-- One row per market per state. `published` is what every page reads;
-- `staging` is the candidate being built, and the primary key is what keeps
-- the two from ever being confused for each other.
--
-- The market's components are columns rather than a key to be parsed: a card
-- needs the region, the item and the rank to render, and a page that had to
-- split a string to find them would have traded one reduction for another.
-- `market_key` is the canonical opaque form, for a cache key or a work
-- partition (Phase 4).
CREATE TABLE market_current (
    market_key   TEXT    NOT NULL,
    state        TEXT    NOT NULL CHECK (state IN ('published', 'staging')),
    version      INTEGER NOT NULL,
    kind         TEXT    NOT NULL CHECK (kind IN ('commodity', 'recipe', 'boe')),
    region       TEXT    NOT NULL,
    item_id      INTEGER NOT NULL,
    -- Commodity only.
    rank         INTEGER,
    -- Per-realm only.
    realm_id     INTEGER,
    -- BoE only, and null for a track no catalogue names.
    track        TEXT,

    observed_at  INTEGER,
    price        INTEGER NOT NULL,
    min_price    INTEGER NOT NULL,
    median_price INTEGER NOT NULL,
    quantity     INTEGER NOT NULL,
    listings     INTEGER NOT NULL,

    first_seen   INTEGER,
    samples      INTEGER NOT NULL,
    mean         INTEGER NOT NULL,
    median       INTEGER NOT NULL,
    low          INTEGER NOT NULL,
    low_at       INTEGER NOT NULL,
    high         INTEGER NOT NULL,
    high_at      INTEGER NOT NULL,
    swing        INTEGER NOT NULL,

    day_percent   INTEGER NOT NULL,
    day_known     INTEGER NOT NULL,
    week_percent  INTEGER NOT NULL,
    week_known    INTEGER NOT NULL,
    month_percent INTEGER NOT NULL,
    month_known   INTEGER NOT NULL,

    best_hour    INTEGER,
    best_weekday INTEGER,
    -- Chart-ready and already thinned, as JSON. Changing the shape of these
    -- is a wire change: CLAUDE.md §9's rule about serde representations
    -- applies here as much as to a job spec.
    by_hour      TEXT NOT NULL DEFAULT '[]',
    by_weekday   TEXT NOT NULL DEFAULT '[]',
    series       TEXT NOT NULL DEFAULT '[]',

    PRIMARY KEY (market_key, state)
) WITHOUT ROWID;

-- Every category page is "the published markets of this kind in this region",
-- so that is what the index leads with. Ending in `item_id` is what lets the
-- page take them already ordered.
CREATE INDEX idx_market_current_page
    ON market_current(state, kind, region, item_id);

-- One row per market per window. A window with no observations gets no row
-- rather than a row of zeroes: an unavailable fact is rendered unavailable,
-- and a stored zero is a price somebody eventually plots.
CREATE TABLE market_windows (
    market_key       TEXT    NOT NULL,
    window           TEXT    NOT NULL,
    state            TEXT    NOT NULL CHECK (state IN ('published', 'staging')),
    version          INTEGER NOT NULL,
    kind             TEXT    NOT NULL,
    region           TEXT    NOT NULL,
    item_id          INTEGER NOT NULL,
    realm_id         INTEGER,

    low              INTEGER NOT NULL,
    low_at           INTEGER NOT NULL,
    high             INTEGER NOT NULL,
    high_at          INTEGER NOT NULL,
    mean             INTEGER NOT NULL,
    median           INTEGER NOT NULL,
    samples          INTEGER NOT NULL,
    first_at         INTEGER NOT NULL,
    last_at          INTEGER NOT NULL,
    -- Data quality, stored rather than implied. Null expected buckets means
    -- the window has no datable start, which is not the same as zero.
    expected_buckets INTEGER,
    observed_buckets INTEGER NOT NULL,
    largest_gap_ms   INTEGER NOT NULL,

    PRIMARY KEY (market_key, window, state)
) WITHOUT ROWID;

-- A card reads one window for every market on the page.
CREATE INDEX idx_market_windows_page
    ON market_windows(state, window, kind, region, item_id);
