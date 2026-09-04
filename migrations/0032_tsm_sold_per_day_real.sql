-- TSM publishes fractional average daily sales volumes (for example, 0.001).
-- Rebuild this small source table so SQLite records them as REAL values.

ALTER TABLE tsm_region_daily RENAME TO tsm_region_daily_old;

CREATE TABLE tsm_region_daily (
    item_id        INTEGER NOT NULL,
    region         TEXT    NOT NULL,
    day            INTEGER NOT NULL,
    market_value   INTEGER NOT NULL,
    historical     INTEGER NOT NULL,
    avg_sale_price INTEGER NOT NULL,
    sale_rate_bp   INTEGER NOT NULL CHECK (sale_rate_bp BETWEEN 0 AND 10000),
    sold_per_day   REAL    NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (item_id, region, day)
) WITHOUT ROWID;

INSERT INTO tsm_region_daily
    SELECT item_id, region, day, market_value, historical, avg_sale_price,
           sale_rate_bp, sold_per_day, updated_at
      FROM tsm_region_daily_old;

DROP TABLE tsm_region_daily_old;

CREATE INDEX idx_tsm_region_daily_region_day
    ON tsm_region_daily(region, day DESC);
