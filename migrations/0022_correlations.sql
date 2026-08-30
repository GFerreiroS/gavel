-- How a market moves, and what it moves with.
--
-- CLAUDE.md §16's Phase 8. All of these are over the *whole* history rather
-- than a window: a weekly rhythm needs weeks, and an association from one
-- afternoon is a shape in noise. The evidence gates inside each measure are
-- what refuse when there is not enough, which is why they are nullable here --
-- a null is "not enough to say", and it is a different thing from a zero.
--
-- `heatmap` is 168 median prices, comma separated, empty for an hour of the
-- week nothing was ever collected in, with the observation count after a `;`.
-- A grid that could not say how much is behind it would fail its own evidence
-- gate on the way back out of storage.
ALTER TABLE market_current ADD COLUMN heatmap TEXT NOT NULL DEFAULT '';

-- Spearman's rho between price and *listed stock*, scaled to -100..100. Not
-- sales volume (§15), and never rendered as causation: `market::correlate`
-- owns the wording so that it cannot drift into a claim in one template.
ALTER TABLE market_current ADD COLUMN stock_rho     INTEGER;
ALTER TABLE market_current ADD COLUMN stock_pairs   INTEGER;

-- The worst fall from a running peak and the best rise from a running trough,
-- as percentages. Properties of the *path*: up-then-down and down-then-up have
-- the same high and low and are opposite things to somebody holding stock.
ALTER TABLE market_current ADD COLUMN drawdown_percent INTEGER NOT NULL DEFAULT 0;
ALTER TABLE market_current ADD COLUMN rise_percent     INTEGER NOT NULL DEFAULT 0;

-- The median absolute change between consecutive observations. Movement, not
-- spread: a market drifting from 100 to 200 is calm and one alternating
-- 140/160 is not, and a spread would rank them the wrong way round.
ALTER TABLE market_current ADD COLUMN typical_move_percent INTEGER;
ALTER TABLE market_current ADD COLUMN stability_changes    INTEGER;

-- Per-realm availability and dispersion (§16, Phase 8).
--
-- `realms_collected` is the denominator `realms` is a fraction of: "listed on
-- 40 realms" means one thing out of 45 collected and another out of 184, and
-- without the denominator the numerator is a number the reader has to go and
-- look up.
--
-- The spread columns are the engine's five-number summary over what each realm
-- charges for its cheapest copy -- how far apart the realms are, which is what
-- "is it worth flying somewhere" reduces to. Computed by the same
-- `Distribution::of` a commodity's history goes through: a different sample,
-- not a different method.
ALTER TABLE market_rollup ADD COLUMN realms_collected INTEGER NOT NULL DEFAULT 0;
ALTER TABLE market_rollup ADD COLUMN spread_p05    INTEGER;
ALTER TABLE market_rollup ADD COLUMN spread_p25    INTEGER;
ALTER TABLE market_rollup ADD COLUMN spread_median INTEGER;
ALTER TABLE market_rollup ADD COLUMN spread_p75    INTEGER;
ALTER TABLE market_rollup ADD COLUMN spread_p95    INTEGER;
ALTER TABLE market_rollup ADD COLUMN spread_iqr    INTEGER;
ALTER TABLE market_rollup ADD COLUMN spread_mad    INTEGER;
ALTER TABLE market_rollup ADD COLUMN spread_realms INTEGER;
