-- WoW Token prices are region-scoped, like commodities, but are not part of
-- the curated catalogue or its materialised market read model.
CREATE TABLE wow_token_prices (
    region      TEXT    NOT NULL,
    observed_at INTEGER NOT NULL,
    price       INTEGER NOT NULL,
    PRIMARY KEY (region, observed_at)
) WITHOUT ROWID;

CREATE INDEX idx_wow_token_prices_region_time
    ON wow_token_prices(region, observed_at DESC);
