-- Where each catalogue is in its life: draft_ptr -> active -> archived.
--
-- The shipped catalogs.json still says what a catalogue *starts* as. This
-- table says what it *is*, because activating a reviewed PTR catalogue is
-- something an administrator does to a running instance (docs/market-analysis
-- §8) and a state compiled into the binary would mean a redeploy to follow a
-- PTR schedule that slipped.
CREATE TABLE catalog_releases (
    catalog_id   TEXT    NOT NULL PRIMARY KEY,
    state        TEXT    NOT NULL CHECK (state IN ('draft_ptr', 'active', 'archived')),
    -- When this row last moved, whatever it moved to.
    changed_at   INTEGER NOT NULL,
    -- Kept separately from changed_at so the archive can say when a tier was
    -- current, not merely that it is not any more.
    activated_at INTEGER,
    archived_at  INTEGER
) WITHOUT ROWID;

-- At most one active catalogue, enforced here rather than assumed by a
-- template. §8 wants activation and archiving to be one transaction precisely
-- so there is never zero or two; a partial unique index is what makes "two"
-- impossible even if the transaction is ever got wrong.
--
-- Note what it does not constrain: zero active catalogues is a legal state.
-- An expansion that has ended and whose successor is still on the PTR is
-- exactly that, and the pages say "archived" rather than breaking.
CREATE UNIQUE INDEX idx_catalog_releases_one_active
    ON catalog_releases(state) WHERE state = 'active';
