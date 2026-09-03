-- Drop strictly redundant indexes.
-- idx_realm_prices_item is redundant with the primary key.
-- idx_realm_prices_window is redundant with the primary key.
DROP INDEX idx_realm_prices_item;
DROP INDEX idx_realm_prices_window;
