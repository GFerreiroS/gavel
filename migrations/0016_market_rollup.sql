-- A region's -- or one realm's -- worth of one per-realm market.
--
-- A commodity market is one price for a region, so `market_current` is the
-- whole answer for it. A gear or recipe market is one price *per connected
-- realm*, and both the card and the analysis page ask about a region's worth
-- of them at once: "the cheapest Veteran copy anywhere in EU, at what item
-- levels, with how many listings behind it". That is a roll-up over markets
-- rather than a market, so it is a row of its own -- docs/market-analysis
-- calls these category-card facts, and §3 keeps MarketKey for real markets.
--
-- The same row shape serves one realm, because "one realm" is the same
-- question with one market in it. That is what stops the page having two
-- implementations of everything it shows.
CREATE TABLE market_rollup (
    region        TEXT    NOT NULL,
    item_id       INTEGER NOT NULL,
    -- The upgrade track's slug; '-' where no catalogue names it, and '' for a
    -- recipe, which has one version of itself.
    track         TEXT    NOT NULL,
    -- 0 means every connected realm in the region. A sentinel rather than a
    -- nullable column because this is half a primary key, and SQLite treats
    -- NULLs in a unique index as distinct -- which would let one region's
    -- roll-up be written twice. Realm ids start well above zero.
    realm_id      INTEGER NOT NULL,
    state         TEXT    NOT NULL CHECK (state IN ('published', 'staging')),
    version       INTEGER NOT NULL,
    kind          TEXT    NOT NULL CHECK (kind IN ('boe', 'recipe')),
    window        TEXT    NOT NULL,

    observed_at   INTEGER,
    snapshots     INTEGER NOT NULL,
    realms        INTEGER NOT NULL,
    -- Three questions, and a page asks all three. A realm's price is its
    -- cheapest copy, so the cheapest, the median and the dearest of those
    -- describe the spread *across realms* -- which realm to fly to. The
    -- dearest listing is the spread *within* the market, which is all there is
    -- once a realm is chosen.
    cheapest_now  INTEGER,
    cheapest_realm INTEGER,
    dearest_realm_now INTEGER,
    dearest_realm INTEGER,
    median_realm_now INTEGER,
    highest_now   INTEGER,
    cheapest_ever INTEGER,
    highest_ever  INTEGER,
    listings_now  INTEGER NOT NULL,
    -- Every listing seen across the window: the denominator a modifier's share
    -- is a share of.
    listings_seen INTEGER NOT NULL,

    level_range   TEXT    NOT NULL DEFAULT '',
    levels        TEXT    NOT NULL DEFAULT '[]',
    modifiers     TEXT    NOT NULL DEFAULT '[]',
    -- One point per snapshot, already thinned. Both charts on the analysis
    -- page are drawn from this one series.
    series        TEXT    NOT NULL DEFAULT '[]',

    PRIMARY KEY (region, item_id, track, realm_id, state)
) WITHOUT ROWID;

-- A card page is "every published roll-up of this kind, in this region, at
-- this scope". Ending in `item_id` is what lets the page take them ordered.
CREATE INDEX idx_market_rollup_page
    ON market_rollup(state, kind, region, realm_id, item_id);
