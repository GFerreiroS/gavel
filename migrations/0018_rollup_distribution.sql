-- The engine's statistics on a per-realm roll-up, so a gear card and a
-- consumable card mean the same thing by "cheap".
--
-- A per-realm market had its own reduction and no percentile at all before
-- Phase 5. These are the same columns `market_windows` carries, over the same
-- equal-duration buckets: one engine, one definition, whichever kind of
-- auction house the market lives in.
ALTER TABLE market_rollup ADD COLUMN p05 INTEGER;
ALTER TABLE market_rollup ADD COLUMN p25 INTEGER;
ALTER TABLE market_rollup ADD COLUMN median INTEGER;
ALTER TABLE market_rollup ADD COLUMN p75 INTEGER;
ALTER TABLE market_rollup ADD COLUMN p95 INTEGER;
ALTER TABLE market_rollup ADD COLUMN iqr INTEGER;
ALTER TABLE market_rollup ADD COLUMN mad INTEGER;
-- Hourly buckets, not snapshots: a realm collected twice in an hour is one
-- hour of evidence.
ALTER TABLE market_rollup ADD COLUMN buckets INTEGER;
ALTER TABLE market_rollup ADD COLUMN swing INTEGER NOT NULL DEFAULT 0;

ALTER TABLE market_rollup ADD COLUMN rank INTEGER;
ALTER TABLE market_rollup ADD COLUMN valuation TEXT;
ALTER TABLE market_rollup ADD COLUMN from_median_percent INTEGER;
ALTER TABLE market_rollup ADD COLUMN anomaly TEXT NOT NULL DEFAULT 'Ordinary';
ALTER TABLE market_rollup ADD COLUMN insufficient TEXT;
ALTER TABLE market_rollup ADD COLUMN insufficient_have INTEGER;
ALTER TABLE market_rollup ADD COLUMN insufficient_need INTEGER;
