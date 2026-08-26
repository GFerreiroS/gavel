-- V0 schema.
--
-- Notes:
--  * Times are milliseconds since the Unix epoch (see cluster_core::Millis).
--  * Job/task specifications are stored as JSON so the spec enum can grow
--    without a migration; everything queried or filtered on gets its own
--    column.
--  * No SQLite-specific type is exposed above the `storage` crate.

CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE linked_accounts (
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider     TEXT    NOT NULL,
    external_id  TEXT    NOT NULL,
    display_name TEXT    NOT NULL,
    linked_at    INTEGER NOT NULL,
    PRIMARY KEY (user_id, provider)
);

CREATE TABLE sessions (
    id         TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);

CREATE TABLE jobs (
    id              INTEGER PRIMARY KEY,
    kind            TEXT    NOT NULL,
    spec_json       TEXT    NOT NULL,
    state           TEXT    NOT NULL,
    task_count      INTEGER NOT NULL,
    tasks_completed INTEGER NOT NULL DEFAULT 0,
    tasks_failed    INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    finished_at     INTEGER
);
CREATE INDEX idx_jobs_created ON jobs(created_at DESC);
CREATE INDEX idx_jobs_state ON jobs(state);

CREATE TABLE tasks (
    id          INTEGER PRIMARY KEY,
    job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    idx         INTEGER NOT NULL,
    spec_json   TEXT    NOT NULL,
    state       TEXT    NOT NULL,
    assigned_to INTEGER,
    attempt     INTEGER NOT NULL DEFAULT 0,
    output      TEXT,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX idx_tasks_job ON tasks(job_id, idx);
CREATE INDEX idx_tasks_state ON tasks(state);

-- One row per failed attempt, never updated: the UI must be able to show the
-- whole failure history of a task.
CREATE TABLE task_failures (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL,
    job_id  INTEGER NOT NULL,
    node_id INTEGER,
    attempt INTEGER NOT NULL,
    at      INTEGER NOT NULL,
    reason  TEXT    NOT NULL,
    detail  TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX idx_failures_job ON task_failures(job_id);
CREATE INDEX idx_failures_task ON task_failures(task_id);

CREATE TABLE cluster_events (
    seq          INTEGER PRIMARY KEY,
    at           INTEGER NOT NULL,
    kind         TEXT    NOT NULL,
    node_id      INTEGER,
    payload_json TEXT    NOT NULL
);
CREATE INDEX idx_events_seq ON cluster_events(seq DESC);

-- Generic byte store behind app_core::repo::KeyValueStore.
CREATE TABLE kv (
    key        TEXT PRIMARY KEY,
    value      BLOB NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Short-lived upstream API responses.
CREATE TABLE cache (
    key        TEXT PRIMARY KEY,
    value      BLOB NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_cache_expires ON cache(expires_at);

-- Monotonic id allocation for job/task ids, which are cluster-wide concepts
-- rather than table rowids.
CREATE TABLE sequences (
    name  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT INTO sequences(name, value) VALUES ('job', 0), ('task', 0), ('event', 0);
