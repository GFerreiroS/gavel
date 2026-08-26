-- Auction-house price history.
--
-- Commodities are region-wide, so there is no realm column: an EU price is
-- THE EU price. When non-commodity items are added they will need their own
-- table with a connected_realm_id, because those markets are per-realm.
--
-- Prices are integer copper. No floating point touches the money path.

CREATE TABLE price_samples (
    item_id      INTEGER NOT NULL,
    region       TEXT    NOT NULL,
    -- Blizzard's Last-Modified, not our clock: snapshots are generated hourly
    -- and we want samples to land on their real instant.
    observed_at  INTEGER NOT NULL,
    min_unit     INTEGER NOT NULL,
    p05_unit     INTEGER NOT NULL,
    median_unit  INTEGER NOT NULL,
    quantity     INTEGER NOT NULL,
    listings     INTEGER NOT NULL,
    -- A retried collection must not double-count an hour.
    PRIMARY KEY (item_id, region, observed_at)
) WITHOUT ROWID;

-- The two access patterns: one item's history, and everything's latest.
CREATE INDEX idx_prices_item_time ON price_samples(item_id, region, observed_at DESC);
CREATE INDEX idx_prices_time ON price_samples(region, observed_at DESC);

CREATE TABLE price_alerts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id     INTEGER NOT NULL,
    region      TEXT    NOT NULL,
    severity    TEXT    NOT NULL,
    observed_at INTEGER NOT NULL,
    current_c   INTEGER NOT NULL,
    baseline_c  INTEGER NOT NULL,
    threshold_c INTEGER NOT NULL,
    discount    INTEGER NOT NULL,
    quantity    INTEGER NOT NULL
);
CREATE INDEX idx_alerts_time ON price_alerts(observed_at DESC);
CREATE INDEX idx_alerts_item ON price_alerts(item_id, region, observed_at DESC);
