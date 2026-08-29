-- The dearest listing of a variant, not just the cheapest.
--
-- 0004 stored min and median only, which answers "what does this cost" but not
-- "what is the spread" -- and on a single realm, where there is no other realm
-- to compare against, the spread is the only comparison left.
--
-- Rows written before this column existed carry 0, which the UI reads as
-- "unknown" rather than as a price of nothing. They fill in as each realm's
-- next snapshot lands, within the hour.
ALTER TABLE realm_price_samples ADD COLUMN max_price INTEGER NOT NULL DEFAULT 0;
