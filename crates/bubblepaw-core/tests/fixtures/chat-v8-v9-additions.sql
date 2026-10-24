-- Frozen chat record steps 8 and 9: durable request usage and account identity.
CREATE TABLE request_usage(
    request_id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    session_id TEXT NOT NULL,
    run_id TEXT,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('main','summary_fork','decider')),
    input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),
    cache_read_tokens INTEGER NOT NULL CHECK(cache_read_tokens>=0),
    cache_creation_tokens INTEGER NOT NULL CHECK(cache_creation_tokens>=0),
    output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),
    reasoning_tokens INTEGER CHECK(reasoning_tokens>=0),
    failed INTEGER NOT NULL CHECK(failed IN (0,1))
);
CREATE INDEX request_usage_session_time ON request_usage(session_id,created_at);
CREATE INDEX request_usage_time ON request_usage(created_at);
ALTER TABLE request_usage ADD COLUMN account TEXT NOT NULL DEFAULT '';
UPDATE request_usage SET account=provider;
DROP INDEX IF EXISTS one_running;
CREATE UNIQUE INDEX one_running ON runs(session_id)
    WHERE status IN ('running','waiting_for_quota');
PRAGMA user_version=9;
