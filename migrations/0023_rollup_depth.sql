-- What it costs to sweep a per-realm market.
--
-- The commodity side has carried this since Phase 7 (`market_current.depth`);
-- the per-realm side collected ladders and never rolled them up, which is the
-- loose end Phase 7 recorded as "wired but not yet shown".
--
-- NULL at region scope, and that is a statement rather than a gap: a sweep
-- happens in one auction house, so pooling ninety realms' supply would quote a
-- price for an order nobody can fill. NULL on a realm too until ladder
-- collection has run for it.
--
-- JSON, like every other stored reduction here, so a shape change is a
-- decode-side decision rather than a migration.
ALTER TABLE market_rollup ADD COLUMN depth TEXT;

-- The ladder those figures were taken from, so the curve on the page and the
-- numbers beside it cannot disagree.
--
-- Merged once, on the write path: a track is sold on a realm as several
-- variants and §8 pools them, so drawing it would mean merging again in the
-- handler and two merges are two chances to drift. Cheap here in a way it is
-- not for commodities -- a BoE market's median is five rungs against a
-- commodity's 127 -- which is why this is stored and `market_current.ladder`
-- is the only other place one lives.
ALTER TABLE market_rollup ADD COLUMN ladder TEXT;
