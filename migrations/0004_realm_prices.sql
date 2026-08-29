-- Per-realm auction history: bind-on-equip gear, and later recipes.
--
-- A separate table from price_samples, as 0003 said it would need to be. The
-- two markets share no column meanings: a commodity has a region-wide unit
-- price, a piece of gear has a buyout on one realm, at one of several item
-- levels that trade under a single item id.
--
-- `variant` is the auction's bonus id list, comma separated and sorted. It is
-- the market's identity: item level, sockets and tertiaries are all functions
-- of it. Blizzard publishes no bonus-id-to-item-level table, so the tiers a
-- reader sees are derived from this at read time -- which means a patch that
-- renumbers bonus ids costs a display rule, never the history.
--
-- Prices are integer copper, as everywhere else. No floating point.

CREATE TABLE realm_price_samples (
    item_id      INTEGER NOT NULL,
    region       TEXT    NOT NULL,
    -- Connected realm. Unique only within a region, hence both in the key.
    realm_id     INTEGER NOT NULL,
    variant      TEXT    NOT NULL,
    -- Blizzard's Last-Modified for that realm's snapshot, not our clock.
    observed_at  INTEGER NOT NULL,
    -- The cheapest way to own one. With a handful of listings per variant a
    -- percentile would be noise dressed as precision, so this is a plain min.
    min_price    INTEGER NOT NULL,
    median_price INTEGER NOT NULL,
    listings     INTEGER NOT NULL,
    -- A retried collection must not double-count a snapshot.
    PRIMARY KEY (item_id, region, realm_id, variant, observed_at)
) WITHOUT ROWID;

-- One item's history on one realm, newest first: the item page.
CREATE INDEX idx_realm_prices_item ON realm_price_samples(item_id, region, realm_id, observed_at DESC);
-- Everything's latest in a region: the cross-realm view, which is the default
-- landing state and therefore the query that has to stay quick.
CREATE INDEX idx_realm_prices_latest ON realm_price_samples(region, observed_at DESC);

-- Which realms we collect, and what to call them. Written at startup from
-- configuration so the UI can name a realm without a second API call, and so
-- a realm dropped from the config keeps its history readable.
CREATE TABLE realms (
    realm_id   INTEGER NOT NULL,
    region     TEXT    NOT NULL,
    name       TEXT    NOT NULL,
    PRIMARY KEY (realm_id, region)
) WITHOUT ROWID;
