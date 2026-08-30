-- What was actually on the shelf, and at what prices.
--
-- CLAUDE.md §16's Phase 7. Every price this app stored before answers "what
-- does one cost"; none of them answers "what does it cost me to buy twenty",
-- which is the question a buyer actually has. That answer needs the ladder --
-- the supply grouped by price -- and the ladder was being thrown away:
-- `stats::summarise` reduced the listings to five numbers and dropped them.
--
-- **This cannot be backfilled.** Four months of archive have no ladders in
-- them and never will, which is why collection lands before the analyses that
-- read it: the data has to start accumulating before it can be studied.
--
-- `steps` is `price:quantity` per rung, `,` between them, cheapest first. The
-- running total is not stored -- it is a sum of what is here, and storing it
-- would double the column to save an addition.
CREATE TABLE price_ladders (
    item_id     INTEGER NOT NULL,
    region      TEXT    NOT NULL,
    observed_at INTEGER NOT NULL,
    -- Rungs and units, denormalised so that "is this market thin" is
    -- answerable without parsing the ladder. The card path asks exactly that
    -- and nothing else.
    levels      INTEGER NOT NULL,
    total       INTEGER NOT NULL,
    steps       TEXT    NOT NULL,
    PRIMARY KEY (item_id, region, observed_at)
) WITHOUT ROWID;

-- The per-realm half. A BoE ladder is four auctions of one item each, so these
-- rows are tiny and there are a great many of them -- the opposite shape from
-- a commodity ladder, which is why `depth::Ladder::is_sparse` exists.
CREATE TABLE realm_price_ladders (
    item_id     INTEGER NOT NULL,
    region      TEXT    NOT NULL,
    realm_id    INTEGER NOT NULL,
    variant     TEXT    NOT NULL,
    observed_at INTEGER NOT NULL,
    levels      INTEGER NOT NULL,
    total       INTEGER NOT NULL,
    steps       TEXT    NOT NULL,
    PRIMARY KEY (item_id, region, realm_id, variant, observed_at)
) WITHOUT ROWID;

-- Retention reads by age and nothing else, so that is what is indexed. The
-- primary keys above lead with the item, which is the wrong end for a sweep
-- that wants "everything before this instant".
CREATE INDEX idx_price_ladders_age       ON price_ladders (observed_at);
CREATE INDEX idx_realm_price_ladders_age ON realm_price_ladders (observed_at);

-- The newest ladder of each market, and what it means for a buyer, on the row
-- every page already reads. Both empty until collection has run: an archive
-- gathered before Phase 7 has no ladders and cannot be given any, so the panel
-- says so rather than drawing an empty market (§2).
--
-- `depth` is the swept summary rather than a set of columns because it is read
-- as a unit and never filtered on -- and because the sweep depends on the
-- catalogue's target quantity, which the storage layer has no business
-- knowing. The card query does not select either of these.
ALTER TABLE market_current ADD COLUMN ladder TEXT NOT NULL DEFAULT '';
ALTER TABLE market_current ADD COLUMN depth  TEXT NOT NULL DEFAULT '';
