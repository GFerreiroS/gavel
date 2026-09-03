-- Store an auction variant once, rather than repeating its comma-separated
-- bonus list in every per-realm sample, ladder, and index entry.
--
-- `variant` remains the domain identity. The SQLite adapter resolves this
-- dictionary internally and continues returning the exact string to callers.

CREATE TABLE market_variants (
    variant_id INTEGER PRIMARY KEY,
    variant    TEXT NOT NULL UNIQUE
);

-- A ladder may exist without its matching summary after an interrupted old
-- deployment, so take the union of both source tables rather than assuming one
-- is a complete vocabulary.
INSERT INTO market_variants (variant)
SELECT variant FROM realm_price_samples
UNION
SELECT variant FROM realm_price_ladders;

CREATE TABLE realm_price_samples_new (
    item_id      INTEGER NOT NULL,
    region       TEXT    NOT NULL,
    realm_id     INTEGER NOT NULL,
    variant_id   INTEGER NOT NULL REFERENCES market_variants(variant_id),
    observed_at  INTEGER NOT NULL,
    min_price    INTEGER NOT NULL,
    median_price INTEGER NOT NULL,
    max_price    INTEGER NOT NULL,
    listings     INTEGER NOT NULL,
    PRIMARY KEY (item_id, region, realm_id, variant_id, observed_at)
) WITHOUT ROWID;

INSERT INTO realm_price_samples_new
    (item_id, region, realm_id, variant_id, observed_at,
     min_price, median_price, max_price, listings)
SELECT samples.item_id, samples.region, samples.realm_id, variants.variant_id,
       samples.observed_at, samples.min_price, samples.median_price,
       samples.max_price, samples.listings
  FROM realm_price_samples AS samples
  JOIN market_variants AS variants ON variants.variant = samples.variant;

DROP TABLE realm_price_samples;
ALTER TABLE realm_price_samples_new RENAME TO realm_price_samples;

CREATE INDEX idx_realm_prices_item
    ON realm_price_samples(item_id, region, realm_id, observed_at DESC);
CREATE INDEX idx_realm_prices_latest
    ON realm_price_samples(region, observed_at DESC);
CREATE INDEX idx_realm_prices_window
    ON realm_price_samples(region, item_id, realm_id, variant_id, observed_at DESC);

CREATE TABLE realm_price_ladders_new (
    item_id      INTEGER NOT NULL,
    region       TEXT    NOT NULL,
    realm_id     INTEGER NOT NULL,
    variant_id   INTEGER NOT NULL REFERENCES market_variants(variant_id),
    observed_at  INTEGER NOT NULL,
    levels       INTEGER NOT NULL,
    total        INTEGER NOT NULL,
    steps        TEXT    NOT NULL,
    PRIMARY KEY (item_id, region, realm_id, variant_id, observed_at)
) WITHOUT ROWID;

INSERT INTO realm_price_ladders_new
    (item_id, region, realm_id, variant_id, observed_at, levels, total, steps)
SELECT ladders.item_id, ladders.region, ladders.realm_id, variants.variant_id,
       ladders.observed_at, ladders.levels, ladders.total, ladders.steps
  FROM realm_price_ladders AS ladders
  JOIN market_variants AS variants ON variants.variant = ladders.variant;

DROP TABLE realm_price_ladders;
ALTER TABLE realm_price_ladders_new RENAME TO realm_price_ladders;

CREATE INDEX idx_realm_price_ladders_age ON realm_price_ladders(observed_at);
