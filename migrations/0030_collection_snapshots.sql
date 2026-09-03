-- The append-only record of snapshots we actually fetched.
--
-- Change rows in realm_price_samples and realm_price_ladders say when a
-- market's state began; this ledger says every time we looked.  It starts
-- empty deliberately: pre-migration rows are observations, not evidence that
-- an unrecorded interval was unchanged.
--
-- `realm_id = 0` reserves the established region-wide sentinel for a future
-- commodity ledger. Migration 0030 writes only non-zero, per-realm rows:
-- commodities changed on 86.7% of measured observations, so their ledger is
-- not worth this task's complexity.
CREATE TABLE collection_snapshots (
    region      TEXT    NOT NULL,
    realm_id    INTEGER NOT NULL,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (region, realm_id, observed_at)
) WITHOUT ROWID;
