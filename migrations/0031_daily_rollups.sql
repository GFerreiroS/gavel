-- Perpetual daily read models.  These are deliberately separate from source
-- observations: §8 keeps raw history forever and makes this table rebuildable.
--
-- `price_daily` is commodities (raw hourly observations); `realm_price_daily`
-- is gear/recipes (the collection ledger expanded through change rows).
CREATE TABLE price_daily (
    item_id          INTEGER NOT NULL,
    region           TEXT    NOT NULL,
    day              INTEGER NOT NULL,
    open_price       INTEGER NOT NULL,
    close_price      INTEGER NOT NULL,
    low_price        INTEGER NOT NULL,
    low_at           INTEGER NOT NULL,
    high_price       INTEGER NOT NULL,
    high_at          INTEGER NOT NULL,
    mean_price       INTEGER NOT NULL,
    p05_price        INTEGER NOT NULL,
    p25_price        INTEGER NOT NULL,
    median_price     INTEGER NOT NULL,
    p75_price        INTEGER NOT NULL,
    p95_price        INTEGER NOT NULL,
    open_quantity    INTEGER NOT NULL,
    close_quantity   INTEGER NOT NULL,
    mean_quantity    INTEGER NOT NULL,
    open_listings    INTEGER NOT NULL,
    close_listings   INTEGER NOT NULL,
    mean_listings    INTEGER NOT NULL,
    samples          INTEGER NOT NULL,
    observed_buckets INTEGER NOT NULL,
    insufficient     TEXT,
    insufficient_have INTEGER,
    insufficient_need INTEGER,
    PRIMARY KEY (item_id, region, day)
) WITHOUT ROWID;

CREATE TABLE realm_price_daily (
    item_id          INTEGER NOT NULL,
    region           TEXT    NOT NULL,
    realm_id         INTEGER NOT NULL,
    variant_id       INTEGER NOT NULL REFERENCES market_variants(variant_id),
    day              INTEGER NOT NULL,
    open_price       INTEGER NOT NULL,
    close_price      INTEGER NOT NULL,
    low_price        INTEGER NOT NULL,
    low_at           INTEGER NOT NULL,
    high_price       INTEGER NOT NULL,
    high_at          INTEGER NOT NULL,
    mean_price       INTEGER NOT NULL,
    p05_price        INTEGER NOT NULL,
    p25_price        INTEGER NOT NULL,
    median_price     INTEGER NOT NULL,
    p75_price        INTEGER NOT NULL,
    p95_price        INTEGER NOT NULL,
    open_listings    INTEGER NOT NULL,
    close_listings   INTEGER NOT NULL,
    mean_listings    INTEGER NOT NULL,
    samples          INTEGER NOT NULL,
    observed_buckets INTEGER NOT NULL,
    insufficient     TEXT,
    insufficient_have INTEGER,
    insufficient_need INTEGER,
    PRIMARY KEY (item_id, region, realm_id, variant_id, day)
) WITHOUT ROWID;

CREATE INDEX idx_price_daily_region_day ON price_daily(region, day, item_id);
CREATE INDEX idx_realm_price_daily_region_day
    ON realm_price_daily(region, day, item_id, realm_id, variant_id);
