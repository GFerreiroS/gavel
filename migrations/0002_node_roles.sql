-- Role assignments are mutable at runtime (CLAUDE.md 19) and must survive a
-- restart, so a node keeps both its identity and the roles it was given.
--
-- Its own table rather than a blob in `kv`: this is queryable state the UI and
-- future rebalancer will both want to read.

CREATE TABLE node_roles (
    node_id    INTEGER PRIMARY KEY,
    -- cluster_core::RoleSet, a one-byte bitmask.
    roles      INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
