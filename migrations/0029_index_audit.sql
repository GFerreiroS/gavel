-- Drop redundant index unused by callers.
-- idx_realm_prices_window is redundant with the primary key.
DROP INDEX idx_realm_prices_window;
