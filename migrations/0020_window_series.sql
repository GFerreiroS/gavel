-- The analysis page's chart, and the distribution behind its verdict.
--
-- CLAUDE.md §16's Phase 6: "Fixed-resolution chart series for each named
-- window; SVG rendering may stay server-side, but series reduction does not
-- happen during the request." The item page called `downsample` on every view.
-- A small reduction -- and still a reduction, and still per request.
--
-- `series` is 96 slots separated by `;`, each `price,median,p25,p75,quantity,
-- listings` and empty for a slot nothing was collected in. The instants are
-- not stored: a slot's time is `from + index * span / 96` by construction, and
-- ninety-six timestamps would be most of the column saying what one
-- subtraction already says. `histogram` is `lo,hi` and then one hour-count per
-- bin.
--
-- Fixed resolution is what makes these bounded columns rather than ones that
-- grow with the archive behind them: a market with four months of history
-- stores exactly what a market with four days does.
ALTER TABLE market_windows ADD COLUMN series    TEXT NOT NULL DEFAULT '';
ALTER TABLE market_windows ADD COLUMN histogram TEXT NOT NULL DEFAULT '';
