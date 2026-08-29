-- Which realms are actually collected, decided at runtime.
--
-- Until now the realm list came from a command-line flag, which meant changing
-- it was a restart and a deploy. A tracker whose scope is a deployment concern
-- cannot be adjusted by the person watching the prices, so the decision moves
-- into the database where an admin page can reach it.
--
-- Every realm the instance knows about has a row. `enabled` says whether it is
-- collected; a realm switched off keeps its history and simply stops growing
-- it, which is what makes turning one off a safe thing to try.
ALTER TABLE realms ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;

-- The collection loop asks for the enabled ones on every cycle.
CREATE INDEX idx_realms_enabled ON realms(enabled);
