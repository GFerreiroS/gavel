-- The engine's statistics, on every window.
--
-- docs/market-analysis.md §5. Before this a window carried a mean, a raw
-- median and the extremes; a card, an analysis page and an alert each decided
-- for themselves what "cheap" meant. These columns are the one answer:
-- Hyndman-Fan R8 percentiles over equal-duration buckets, the robust spreads
-- beside them, and the evidence that decides whether a band may be shown at
-- all.
--
-- `median` already existed and now holds the engine's, which is a different
-- number from the raw one it held before -- the same reason every other column
-- here is new rather than reused.
ALTER TABLE market_windows ADD COLUMN p05 INTEGER NOT NULL DEFAULT 0;
ALTER TABLE market_windows ADD COLUMN p25 INTEGER NOT NULL DEFAULT 0;
ALTER TABLE market_windows ADD COLUMN p75 INTEGER NOT NULL DEFAULT 0;
ALTER TABLE market_windows ADD COLUMN p95 INTEGER NOT NULL DEFAULT 0;
-- P75 - P25, and the median absolute deviation. §5.4's stable spreads.
ALTER TABLE market_windows ADD COLUMN iqr INTEGER NOT NULL DEFAULT 0;
ALTER TABLE market_windows ADD COLUMN mad INTEGER NOT NULL DEFAULT 0;
-- Hourly buckets behind all of the above. Not `samples`, which counts rows:
-- two observations in one hour are one hour of evidence.
ALTER TABLE market_windows ADD COLUMN buckets INTEGER NOT NULL DEFAULT 0;
-- (max - min) / mean, named for what it is rather than called volatility.
ALTER TABLE market_windows ADD COLUMN swing INTEGER NOT NULL DEFAULT 0;

-- Where the market's current price sits in this window. Null where the
-- evidence gate refused, which is a different state from a rank of zero.
ALTER TABLE market_windows ADD COLUMN rank INTEGER;
ALTER TABLE market_windows ADD COLUMN valuation TEXT;
ALTER TABLE market_windows ADD COLUMN from_median_percent INTEGER;
-- Whether the price is far from the body of the distribution, which is a
-- separate statement from the band (§5.4).
ALTER TABLE market_windows ADD COLUMN anomaly TEXT NOT NULL DEFAULT 'Ordinary';
-- Why there is no band, when there is none: the reason and its two numbers, so
-- a page can say "not enough history: 30 hours of 72" rather than go quiet.
ALTER TABLE market_windows ADD COLUMN insufficient TEXT;
ALTER TABLE market_windows ADD COLUMN insufficient_have INTEGER;
ALTER TABLE market_windows ADD COLUMN insufficient_need INTEGER;
