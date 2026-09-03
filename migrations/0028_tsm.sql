-- TradeSkillMaster's independent commodity and completed-sales measurements.
--
-- Kept out of price_samples: their valuations are not ours, and silently
-- blending them would turn a source distinction into a product claim.

CREATE TABLE tsm_region_daily (
    item_id        INTEGER NOT NULL,
    region         TEXT    NOT NULL,
    day            INTEGER NOT NULL,
    market_value   INTEGER NOT NULL,
    historical     INTEGER NOT NULL,
    avg_sale_price INTEGER NOT NULL,
    sale_rate_bp   INTEGER NOT NULL CHECK (sale_rate_bp BETWEEN 0 AND 10000),
    sold_per_day   INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (item_id, region, day)
) WITHOUT ROWID;

CREATE INDEX idx_tsm_region_daily_region_day
    ON tsm_region_daily(region, day DESC);

CREATE TABLE tsm_commodity_sample (
    item_id      INTEGER NOT NULL,
    region       TEXT    NOT NULL,
    observed_at  INTEGER NOT NULL,
    market_value INTEGER NOT NULL,
    min_buyout   INTEGER NOT NULL,
    recent       INTEGER NOT NULL,
    historical   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (item_id, region, observed_at)
) WITHOUT ROWID;

CREATE INDEX idx_tsm_commodity_sample_region_observed
    ON tsm_commodity_sample(region, observed_at DESC);
